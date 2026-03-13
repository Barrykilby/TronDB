//! WalRecord <-> Proto conversions.
//!
//! Implements `From<&WalRecord> for pb::WalRecordMessage` and
//! `TryFrom<pb::WalRecordMessage> for WalRecord` with `RecordType` ↔
//! `RecordTypeProto` mapping.

use crate::pb;
use trondb_wal::record::{RecordType, WalRecord};

// ---------------------------------------------------------------------------
// RecordType helpers
// ---------------------------------------------------------------------------

fn record_type_to_proto(rt: RecordType) -> pb::RecordTypeProto {
    match rt {
        RecordType::TxBegin => pb::RecordTypeProto::RecordTypeTxBegin,
        RecordType::TxCommit => pb::RecordTypeProto::RecordTypeTxCommit,
        RecordType::TxAbort => pb::RecordTypeProto::RecordTypeTxAbort,
        RecordType::EntityWrite => pb::RecordTypeProto::RecordTypeEntityWrite,
        RecordType::EntityDelete => pb::RecordTypeProto::RecordTypeEntityDelete,
        RecordType::ReprWrite => pb::RecordTypeProto::RecordTypeReprWrite,
        RecordType::ReprDirty => pb::RecordTypeProto::RecordTypeReprDirty,
        RecordType::EdgeWrite => pb::RecordTypeProto::RecordTypeEdgeWrite,
        RecordType::EdgeInferred => pb::RecordTypeProto::RecordTypeEdgeInferred,
        RecordType::EdgeConfidenceUpdate => pb::RecordTypeProto::RecordTypeEdgeConfidenceUpdate,
        RecordType::EdgeConfirm => pb::RecordTypeProto::RecordTypeEdgeConfirm,
        RecordType::EdgeDelete => pb::RecordTypeProto::RecordTypeEdgeDelete,
        RecordType::LocationUpdate => pb::RecordTypeProto::RecordTypeLocationUpdate,
        RecordType::SchemaCreateColl => pb::RecordTypeProto::RecordTypeSchemaCreateColl,
        RecordType::SchemaCreateEdgeType => pb::RecordTypeProto::RecordTypeSchemaCreateEdgeType,
        RecordType::AffinityGroupCreate => pb::RecordTypeProto::RecordTypeAffinityGroupCreate,
        RecordType::AffinityGroupMember => pb::RecordTypeProto::RecordTypeAffinityGroupMember,
        RecordType::AffinityGroupRemove => pb::RecordTypeProto::RecordTypeAffinityGroupRemove,
        RecordType::TierMigration => pb::RecordTypeProto::RecordTypeTierMigration,
        RecordType::Checkpoint => pb::RecordTypeProto::RecordTypeCheckpoint,
        RecordType::SchemaDropColl => pb::RecordTypeProto::RecordTypeSchemaDropColl,
        RecordType::SchemaDropEdgeType => pb::RecordTypeProto::RecordTypeSchemaDropEdgeType,
    }
}

fn proto_to_record_type(proto: i32) -> Result<RecordType, String> {
    match pb::RecordTypeProto::try_from(proto) {
        Ok(pb::RecordTypeProto::RecordTypeTxBegin) => Ok(RecordType::TxBegin),
        Ok(pb::RecordTypeProto::RecordTypeTxCommit) => Ok(RecordType::TxCommit),
        Ok(pb::RecordTypeProto::RecordTypeTxAbort) => Ok(RecordType::TxAbort),
        Ok(pb::RecordTypeProto::RecordTypeEntityWrite) => Ok(RecordType::EntityWrite),
        Ok(pb::RecordTypeProto::RecordTypeEntityDelete) => Ok(RecordType::EntityDelete),
        Ok(pb::RecordTypeProto::RecordTypeReprWrite) => Ok(RecordType::ReprWrite),
        Ok(pb::RecordTypeProto::RecordTypeReprDirty) => Ok(RecordType::ReprDirty),
        Ok(pb::RecordTypeProto::RecordTypeEdgeWrite) => Ok(RecordType::EdgeWrite),
        Ok(pb::RecordTypeProto::RecordTypeEdgeInferred) => Ok(RecordType::EdgeInferred),
        Ok(pb::RecordTypeProto::RecordTypeEdgeConfidenceUpdate) => {
            Ok(RecordType::EdgeConfidenceUpdate)
        }
        Ok(pb::RecordTypeProto::RecordTypeEdgeConfirm) => Ok(RecordType::EdgeConfirm),
        Ok(pb::RecordTypeProto::RecordTypeEdgeDelete) => Ok(RecordType::EdgeDelete),
        Ok(pb::RecordTypeProto::RecordTypeLocationUpdate) => Ok(RecordType::LocationUpdate),
        Ok(pb::RecordTypeProto::RecordTypeSchemaCreateColl) => Ok(RecordType::SchemaCreateColl),
        Ok(pb::RecordTypeProto::RecordTypeSchemaCreateEdgeType) => {
            Ok(RecordType::SchemaCreateEdgeType)
        }
        Ok(pb::RecordTypeProto::RecordTypeAffinityGroupCreate) => {
            Ok(RecordType::AffinityGroupCreate)
        }
        Ok(pb::RecordTypeProto::RecordTypeAffinityGroupMember) => {
            Ok(RecordType::AffinityGroupMember)
        }
        Ok(pb::RecordTypeProto::RecordTypeAffinityGroupRemove) => {
            Ok(RecordType::AffinityGroupRemove)
        }
        Ok(pb::RecordTypeProto::RecordTypeTierMigration) => Ok(RecordType::TierMigration),
        Ok(pb::RecordTypeProto::RecordTypeCheckpoint) => Ok(RecordType::Checkpoint),
        Ok(pb::RecordTypeProto::RecordTypeSchemaDropColl) => Ok(RecordType::SchemaDropColl),
        Ok(pb::RecordTypeProto::RecordTypeSchemaDropEdgeType) => {
            Ok(RecordType::SchemaDropEdgeType)
        }
        Err(_) => Err(format!("unknown RecordTypeProto value: {proto}")),
    }
}

// ---------------------------------------------------------------------------
// WalRecord -> WalRecordMessage
// ---------------------------------------------------------------------------

impl From<&WalRecord> for pb::WalRecordMessage {
    fn from(record: &WalRecord) -> Self {
        pb::WalRecordMessage {
            lsn: record.lsn,
            timestamp: record.ts,
            tx_id: record.tx_id,
            record_type: record_type_to_proto(record.record_type) as i32,
            schema_ver: record.schema_ver,
            collection: record.collection.clone(),
            payload: record.payload.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// WalRecordMessage -> WalRecord
// ---------------------------------------------------------------------------

impl TryFrom<pb::WalRecordMessage> for WalRecord {
    type Error = String;

    fn try_from(proto: pb::WalRecordMessage) -> Result<Self, Self::Error> {
        Ok(WalRecord {
            lsn: proto.lsn,
            ts: proto.timestamp,
            tx_id: proto.tx_id,
            record_type: proto_to_record_type(proto.record_type)?,
            schema_ver: proto.schema_ver,
            collection: proto.collection,
            payload: proto.payload,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use trondb_wal::record::{RecordType, WalRecord};

    #[test]
    fn wal_record_round_trip() {
        let record = WalRecord {
            lsn: 42,
            ts: 1741612800000,
            tx_id: 100,
            record_type: RecordType::EntityWrite,
            schema_ver: 1,
            collection: "venues".into(),
            payload: vec![1, 2, 3, 4],
        };
        let proto: pb::WalRecordMessage = (&record).into();
        let restored = WalRecord::try_from(proto).unwrap();
        assert_eq!(restored.lsn, 42);
        assert_eq!(restored.ts, 1741612800000);
        assert_eq!(restored.record_type, RecordType::EntityWrite);
        assert_eq!(restored.payload, vec![1, 2, 3, 4]);
    }

    #[test]
    fn timestamp_maps_to_ts() {
        let record = WalRecord {
            lsn: 1,
            ts: 9_999_999_999,
            tx_id: 0,
            record_type: RecordType::TxBegin,
            schema_ver: 0,
            collection: String::new(),
            payload: vec![],
        };
        let proto: pb::WalRecordMessage = (&record).into();
        assert_eq!(proto.timestamp, 9_999_999_999);
        let restored = WalRecord::try_from(proto).unwrap();
        assert_eq!(restored.ts, 9_999_999_999);
    }

    #[test]
    fn all_record_types_round_trip() {
        let variants = [
            RecordType::TxBegin,
            RecordType::TxCommit,
            RecordType::TxAbort,
            RecordType::EntityWrite,
            RecordType::EntityDelete,
            RecordType::ReprWrite,
            RecordType::ReprDirty,
            RecordType::EdgeWrite,
            RecordType::EdgeInferred,
            RecordType::EdgeConfidenceUpdate,
            RecordType::EdgeConfirm,
            RecordType::EdgeDelete,
            RecordType::LocationUpdate,
            RecordType::SchemaCreateColl,
            RecordType::SchemaCreateEdgeType,
            RecordType::AffinityGroupCreate,
            RecordType::AffinityGroupMember,
            RecordType::AffinityGroupRemove,
            RecordType::TierMigration,
            RecordType::Checkpoint,
            RecordType::SchemaDropColl,
            RecordType::SchemaDropEdgeType,
        ];
        for rt in &variants {
            let record = WalRecord {
                lsn: 0,
                ts: 0,
                tx_id: 0,
                record_type: *rt,
                schema_ver: 0,
                collection: String::new(),
                payload: vec![],
            };
            let proto: pb::WalRecordMessage = (&record).into();
            let restored = WalRecord::try_from(proto).unwrap();
            assert_eq!(restored.record_type, *rt, "failed for {rt:?}");
        }
    }

    #[test]
    fn checkpoint_record_round_trip() {
        let record = WalRecord {
            lsn: 999,
            ts: 0,
            tx_id: 0,
            record_type: RecordType::Checkpoint,
            schema_ver: 2,
            collection: "sys".into(),
            payload: vec![0xAB, 0xCD],
        };
        let proto: pb::WalRecordMessage = (&record).into();
        let restored = WalRecord::try_from(proto).unwrap();
        assert_eq!(restored.lsn, 999);
        assert_eq!(restored.schema_ver, 2);
        assert_eq!(restored.record_type, RecordType::Checkpoint);
        assert_eq!(restored.payload, vec![0xAB, 0xCD]);
    }
}
