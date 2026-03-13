use serde::{Deserialize, Serialize};
use crate::error::WalError;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordType {
    TxBegin              = 0x01,
    TxCommit             = 0x02,
    TxAbort              = 0x03,
    EntityWrite          = 0x10,
    EntityDelete         = 0x11,
    ReprWrite            = 0x20,
    ReprDirty            = 0x21,
    EdgeWrite            = 0x30,
    EdgeInferred         = 0x31,
    EdgeConfidenceUpdate = 0x32,
    EdgeConfirm          = 0x33,
    EdgeDelete           = 0x34,
    LocationUpdate       = 0x40,
    SchemaCreateColl     = 0x50,
    SchemaCreateEdgeType = 0x51,
    SchemaDropColl       = 0x52,
    SchemaDropEdgeType   = 0x53,
    SchemaAlter          = 0x54,
    AffinityGroupCreate  = 0x60,
    AffinityGroupMember  = 0x61,
    AffinityGroupRemove  = 0x62,
    TierMigration        = 0x70,
    Checkpoint           = 0xFF,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalRecord {
    pub lsn: u64,
    pub ts: i64,
    pub tx_id: u64,
    pub record_type: RecordType,
    pub schema_ver: u32,
    pub collection: String,
    pub payload: Vec<u8>,
}

impl WalRecord {
    /// Serialise the record body to MessagePack bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, WalError> {
        rmp_serde::to_vec_named(self)
            .map_err(|e| WalError::Serialisation(e.to_string()))
    }

    /// Deserialise a record body from MessagePack bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WalError> {
        rmp_serde::from_slice(bytes)
            .map_err(|e| WalError::Serialisation(e.to_string()))
    }

    /// Serialise to framed format: [4-byte length][body][4-byte CRC32]
    pub fn to_framed_bytes(&self) -> Result<Vec<u8>, WalError> {
        let body = self.to_bytes()?;
        let crc = crc32fast::hash(&body);
        let len = body.len() as u32;

        let mut out = Vec::with_capacity(4 + body.len() + 4);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc.to_be_bytes());
        Ok(out)
    }

    /// Deserialise from framed format, verifying CRC32.
    pub fn from_framed_bytes(bytes: &[u8]) -> Result<Self, WalError> {
        if bytes.len() < 8 {
            return Err(WalError::Truncated(0));
        }

        let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        if bytes.len() < 4 + len + 4 {
            return Err(WalError::Truncated(0));
        }

        let body = &bytes[4..4 + len];
        let expected_crc = u32::from_be_bytes([
            bytes[4 + len],
            bytes[4 + len + 1],
            bytes[4 + len + 2],
            bytes[4 + len + 3],
        ]);

        let actual_crc = crc32fast::hash(body);
        if actual_crc != expected_crc {
            // Try to extract LSN for error reporting
            let lsn = rmp_serde::from_slice::<WalRecord>(body)
                .map(|r| r.lsn)
                .unwrap_or(0);
            return Err(WalError::CrcMismatch {
                lsn,
                expected: expected_crc,
                got: actual_crc,
            });
        }

        Self::from_bytes(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_record_round_trip_msgpack() {
        let record = WalRecord {
            lsn: 1,
            ts: 1741612800000,
            tx_id: 100,
            record_type: RecordType::EntityWrite,
            schema_ver: 1,
            collection: "venues".into(),
            payload: rmp_serde::to_vec(&serde_json::json!({"entity_id": "e1"})).unwrap(),
        };

        let bytes = record.to_bytes().unwrap();
        let decoded = WalRecord::from_bytes(&bytes).unwrap();

        assert_eq!(record.lsn, decoded.lsn);
        assert_eq!(record.tx_id, decoded.tx_id);
        assert_eq!(record.record_type, decoded.record_type);
        assert_eq!(record.collection, decoded.collection);
        assert_eq!(record.payload, decoded.payload);
    }

    #[test]
    fn crc32_detects_corruption() {
        let record = WalRecord {
            lsn: 1,
            ts: 1741612800000,
            tx_id: 100,
            record_type: RecordType::EntityWrite,
            schema_ver: 1,
            collection: "venues".into(),
            payload: vec![1, 2, 3],
        };

        let mut framed = record.to_framed_bytes().unwrap();
        // Corrupt a byte in the body
        framed[6] ^= 0xFF;
        assert!(WalRecord::from_framed_bytes(&framed).is_err());
    }

    #[test]
    fn record_type_discriminants() {
        assert_eq!(RecordType::TxBegin as u8, 0x01);
        assert_eq!(RecordType::TxCommit as u8, 0x02);
        assert_eq!(RecordType::EntityWrite as u8, 0x10);
        assert_eq!(RecordType::Checkpoint as u8, 0xFF);
        assert_eq!(RecordType::SchemaAlter as u8, 0x54);
        assert_eq!(RecordType::AffinityGroupCreate as u8, 0x60);
        assert_eq!(RecordType::AffinityGroupMember as u8, 0x61);
        assert_eq!(RecordType::AffinityGroupRemove as u8, 0x62);
    }
}
