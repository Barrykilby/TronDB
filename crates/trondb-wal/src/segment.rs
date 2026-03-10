use std::path::Path;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt, AsyncSeekExt, BufWriter};

use crate::error::WalError;
use crate::record::WalRecord;

pub const SEGMENT_HEADER_SIZE: usize = 64;
const MAGIC: &[u8; 8] = b"TRONWAL1";
const VERSION: u16 = 1;

#[derive(Debug)]
pub struct SegmentHeader {
    pub magic: [u8; 8],
    pub version: u16,
    pub segment_id: u64,
    pub created_at: i64,
}

pub struct WalSegment {
    writer: BufWriter<File>,
    path: std::path::PathBuf,
    current_size: u64,
}

impl WalSegment {
    /// Create a new segment file with header.
    pub async fn create(path: &Path, segment_id: u64) -> Result<Self, WalError> {
        let file = File::create(path).await?;
        let mut writer = BufWriter::new(file);

        // Write 64-byte header
        let mut header = [0u8; SEGMENT_HEADER_SIZE];
        header[0..8].copy_from_slice(MAGIC);
        header[8..10].copy_from_slice(&VERSION.to_be_bytes());
        header[10..18].copy_from_slice(&segment_id.to_be_bytes());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        header[18..26].copy_from_slice(&now.to_be_bytes());
        // bytes 26..64 are reserved (zeros)

        writer.write_all(&header).await?;
        writer.flush().await?;

        Ok(Self {
            writer,
            path: path.to_path_buf(),
            current_size: SEGMENT_HEADER_SIZE as u64,
        })
    }

    /// Open an existing segment for appending.
    pub async fn open_append(path: &Path) -> Result<Self, WalError> {
        let file = OpenOptions::new().append(true).open(path).await?;
        let size = file.metadata().await?.len();
        Ok(Self {
            writer: BufWriter::new(file),
            path: path.to_path_buf(),
            current_size: size,
        })
    }

    /// Read the segment header.
    pub async fn read_header(path: &Path) -> Result<SegmentHeader, WalError> {
        let mut file = File::open(path).await?;
        let mut buf = [0u8; SEGMENT_HEADER_SIZE];
        file.read_exact(&mut buf).await?;

        let mut magic = [0u8; 8];
        magic.copy_from_slice(&buf[0..8]);
        if &magic != MAGIC {
            return Err(WalError::InvalidHeader(format!(
                "bad magic: expected TRONWAL1, got {:?}",
                &magic
            )));
        }

        Ok(SegmentHeader {
            magic,
            version: u16::from_be_bytes([buf[8], buf[9]]),
            segment_id: u64::from_be_bytes(buf[10..18].try_into().unwrap()),
            created_at: i64::from_be_bytes(buf[18..26].try_into().unwrap()),
        })
    }

    /// Append a framed record to the segment buffer.
    pub async fn append(&mut self, record: &WalRecord) -> Result<usize, WalError> {
        let framed = record.to_framed_bytes()?;
        self.writer.write_all(&framed).await?;
        self.current_size += framed.len() as u64;
        Ok(framed.len())
    }

    /// Flush buffered writes to disk.
    pub async fn flush(&mut self) -> Result<(), WalError> {
        self.writer.flush().await?;
        Ok(())
    }

    /// Flush and fsync to ensure durability.
    pub async fn sync(&mut self) -> Result<(), WalError> {
        self.writer.flush().await?;
        self.writer.get_ref().sync_all().await?;
        Ok(())
    }

    pub fn current_size(&self) -> u64 {
        self.current_size
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read all records from a segment file, verifying CRC on each.
    /// Returns records in LSN order. Stops gracefully on truncated/corrupt tail record.
    pub async fn read_all(path: &Path) -> Result<Vec<WalRecord>, WalError> {
        let mut file = File::open(path).await?;
        let file_len = file.metadata().await?.len();

        // Skip header
        file.seek(std::io::SeekFrom::Start(SEGMENT_HEADER_SIZE as u64)).await?;

        let mut records = Vec::new();
        let mut pos = SEGMENT_HEADER_SIZE as u64;

        while pos < file_len {
            // Read length prefix (4 bytes)
            if pos + 4 > file_len {
                break; // truncated length prefix — expected after crash
            }
            let mut len_buf = [0u8; 4];
            file.read_exact(&mut len_buf).await?;
            let body_len = u32::from_be_bytes(len_buf) as u64;

            // Read body + CRC (body_len bytes + 4-byte CRC32)
            let frame_remaining = body_len + 4;
            if pos + 4 + frame_remaining > file_len {
                break; // truncated record — expected after crash
            }

            let mut frame = vec![0u8; frame_remaining as usize];
            file.read_exact(&mut frame).await?;

            // Verify CRC
            let body = &frame[..body_len as usize];
            let crc_bytes = &frame[body_len as usize..];
            let expected_crc = u32::from_be_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
            let actual_crc = crc32fast::hash(body);

            if actual_crc != expected_crc {
                // If this is the last record in the file, it's an expected crash truncation
                if pos + 4 + frame_remaining >= file_len {
                    break;
                }
                // Mid-file corruption — log and skip
                eprintln!("WAL: CRC mismatch at offset {pos}, skipping record");
                pos += 4 + frame_remaining;
                continue;
            }

            match WalRecord::from_bytes(body) {
                Ok(record) => records.push(record),
                Err(e) => {
                    eprintln!("WAL: failed to deserialise record at offset {pos}: {e}");
                    pos += 4 + frame_remaining;
                    continue;
                }
            }

            pos += 4 + frame_remaining;
        }

        Ok(records)
    }
}

/// Parse segment_id from a segment file name like "wal_0000000000000001.twal".
pub fn segment_id_from_filename(name: &str) -> Option<u64> {
    let name = name.strip_prefix("wal_")?.strip_suffix(".twal")?;
    u64::from_str_radix(name, 16).ok()
}

/// Generate a segment file name from a segment_id.
pub fn segment_filename(segment_id: u64) -> String {
    format!("wal_{segment_id:016x}.twal")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{RecordType, WalRecord};
    use tempfile::TempDir;

    fn make_record(lsn: u64) -> WalRecord {
        WalRecord {
            lsn,
            ts: 1741612800000,
            tx_id: 1,
            record_type: RecordType::EntityWrite,
            schema_ver: 1,
            collection: "test".into(),
            payload: vec![1, 2, 3],
        }
    }

    #[tokio::test]
    async fn write_and_read_segment_header() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wal_0000000000000001.twal");

        WalSegment::create(&path, 1).await.unwrap();
        let header = WalSegment::read_header(&path).await.unwrap();

        assert_eq!(&header.magic, b"TRONWAL1");
        assert_eq!(header.version, 1);
        assert_eq!(header.segment_id, 1);
    }

    #[tokio::test]
    async fn append_and_read_records() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wal_0000000000000001.twal");

        let mut seg = WalSegment::create(&path, 1).await.unwrap();
        seg.append(&make_record(1)).await.unwrap();
        seg.append(&make_record(2)).await.unwrap();
        seg.flush().await.unwrap();

        let records = WalSegment::read_all(&path).await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].lsn, 1);
        assert_eq!(records[1].lsn, 2);
    }

    #[tokio::test]
    async fn segment_reports_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wal_0000000000000001.twal");

        let mut seg = WalSegment::create(&path, 1).await.unwrap();
        let initial = seg.current_size();
        assert_eq!(initial, SEGMENT_HEADER_SIZE as u64);

        seg.append(&make_record(1)).await.unwrap();
        seg.flush().await.unwrap();
        assert!(seg.current_size() > initial);
    }

    #[tokio::test]
    async fn segment_filename_round_trip() {
        let name = segment_filename(1);
        assert_eq!(name, "wal_0000000000000001.twal");
        assert_eq!(segment_id_from_filename(&name), Some(1));

        let name2 = segment_filename(0xdeadbeef_cafebabe);
        assert_eq!(segment_id_from_filename(&name2), Some(0xdeadbeef_cafebabe));
    }

    #[tokio::test]
    async fn read_all_graceful_truncation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wal_0000000000000001.twal");

        // Write 3 records normally
        let mut seg = WalSegment::create(&path, 1).await.unwrap();
        seg.append(&make_record(1)).await.unwrap();
        seg.append(&make_record(2)).await.unwrap();
        seg.append(&make_record(3)).await.unwrap();
        seg.flush().await.unwrap();
        drop(seg);

        // Truncate the file to simulate a crash mid-write of a 4th record
        {
            use std::fs::OpenOptions as StdOpenOptions;
            use std::io::Write;
            let mut f = StdOpenOptions::new().append(true).open(&path).unwrap();
            // Write a partial frame: just the length prefix and some bytes (no CRC)
            f.write_all(&[0, 0, 0, 10, 1, 2, 3]).unwrap(); // partial body, no CRC
            f.flush().unwrap();
        }

        // read_all should return exactly 3 records, ignoring the truncated tail
        let records = WalSegment::read_all(&path).await.unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[2].lsn, 3);
    }

    #[tokio::test]
    async fn open_append_preserves_existing_records() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wal_0000000000000001.twal");

        // Create and write 2 records
        let mut seg = WalSegment::create(&path, 1).await.unwrap();
        seg.append(&make_record(1)).await.unwrap();
        seg.append(&make_record(2)).await.unwrap();
        seg.flush().await.unwrap();
        drop(seg);

        // Re-open for append and write a 3rd
        let mut seg2 = WalSegment::open_append(&path).await.unwrap();
        seg2.append(&make_record(3)).await.unwrap();
        seg2.flush().await.unwrap();
        drop(seg2);

        let records = WalSegment::read_all(&path).await.unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[2].lsn, 3);
    }
}
