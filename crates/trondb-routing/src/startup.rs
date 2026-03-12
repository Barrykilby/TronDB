use trondb_core::Engine;
use trondb_core::error::EngineError;
use trondb_core::location::{Encoding, ReprKey, Tier};
use trondb_wal::RecordType;
use trondb_wal::record::WalRecord;

use crate::affinity::AffinityIndex;
use crate::migrator::{MigrationRecoveryAction, TierMigrationPayload, TierMigrator};

/// Determine the encoding that matches a given tier.
fn encoding_for_tier(tier: Tier) -> Encoding {
    match tier {
        Tier::Ram | Tier::Fjall => Encoding::Float32,
        Tier::NVMe => Encoding::Int8,
        Tier::Archive => Encoding::Binary,
    }
}

/// Process pending WAL records that the core engine could not handle on its
/// own (affinity group mutations and tier migration intents).  Called once at
/// startup, after `Engine::open` returns the unhandled record set.
pub async fn process_pending_wal_records(
    engine: &Engine,
    records: &[WalRecord],
    affinity: &AffinityIndex,
) -> Result<(), EngineError> {
    let max_group_size = 1000;

    for record in records {
        match record.record_type {
            RecordType::AffinityGroupCreate
            | RecordType::AffinityGroupMember
            | RecordType::AffinityGroupRemove => {
                affinity.replay_affinity_record(
                    record.record_type,
                    &record.payload,
                    max_group_size,
                );
            }
            RecordType::TierMigration => {
                let payload: TierMigrationPayload =
                    rmp_serde::from_slice(&record.payload)
                        .map_err(|e| EngineError::Internal(e.to_string()))?;

                let entity_id =
                    trondb_core::types::LogicalId::from_string(&payload.entity_id);
                let source_exists = engine
                    .read_tiered(&payload.collection, &entity_id, payload.from_tier)?
                    .is_some();
                let target_exists = engine
                    .read_tiered(&payload.collection, &entity_id, payload.to_tier)?
                    .is_some();

                match TierMigrator::classify_migration_state(source_exists, target_exists)
                {
                    MigrationRecoveryAction::NoAction => {}
                    MigrationRecoveryAction::AlreadyComplete => {
                        // Data already at target — just reconcile the location table.
                        let enc = encoding_for_tier(payload.to_tier);
                        let key = ReprKey {
                            entity_id: entity_id.clone(),
                            repr_index: 0,
                        };
                        // Location entry may not exist (e.g. first boot); ignore errors.
                        let _ = engine.location().update_tier(&key, payload.to_tier, enc);
                    }
                    MigrationRecoveryAction::CleanupSource => {
                        engine.delete_from_tier(
                            &payload.collection,
                            &entity_id,
                            payload.from_tier,
                        )?;
                    }
                    MigrationRecoveryAction::ReExecute => {
                        if let Some(entity_data) = engine.read_tiered(
                            &payload.collection,
                            &entity_id,
                            payload.from_tier,
                        )? {
                            engine.write_tiered(
                                &payload.collection,
                                &entity_id,
                                payload.to_tier,
                                &entity_data,
                            )?;
                            engine.delete_from_tier(
                                &payload.collection,
                                &entity_id,
                                payload.from_tier,
                            )?;
                            let enc = encoding_for_tier(payload.to_tier);
                            let key = ReprKey {
                                entity_id: entity_id.clone(),
                                repr_index: 0,
                            };
                            let _ =
                                engine.location().update_tier(&key, payload.to_tier, enc);
                        }
                    }
                }
            }
            _ => {} // Unknown records — safe to skip
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use trondb_core::types::LogicalId;

    /// Helper: open a fresh Engine backed by a temp dir.
    async fn make_engine(
        dir: &std::path::Path,
    ) -> (Engine, Vec<WalRecord>) {
        let cfg = trondb_core::EngineConfig {
            data_dir: dir.join("store"),
            wal: trondb_wal::WalConfig {
                wal_dir: dir.join("wal"),
                ..Default::default()
            },
            snapshot_interval_secs: 0,
            hnsw_snapshot_interval_secs: 0,
        };
        trondb_core::Engine::open(cfg).await.unwrap()
    }

    #[tokio::test]
    async fn process_pending_affinity_records() {
        use crate::affinity::{
            AffinityGroupCreatePayload, AffinityGroupMemberPayload,
        };
        use crate::node::AffinityGroupId;

        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = make_engine(dir.path()).await;

        // Build synthetic WAL records for AffinityGroupCreate + AffinityGroupMember
        let create_payload = rmp_serde::to_vec_named(&AffinityGroupCreatePayload {
            group_id: "g1".into(),
        })
        .unwrap();
        let member_payload = rmp_serde::to_vec_named(&AffinityGroupMemberPayload {
            group_id: "g1".into(),
            entity_id: "e1".into(),
        })
        .unwrap();

        let records = vec![
            WalRecord {
                lsn: 1,
                ts: 0,
                tx_id: 1,
                record_type: RecordType::AffinityGroupCreate,
                schema_ver: 1,
                collection: "affinity".into(),
                payload: create_payload,
            },
            WalRecord {
                lsn: 2,
                ts: 0,
                tx_id: 2,
                record_type: RecordType::AffinityGroupMember,
                schema_ver: 1,
                collection: "affinity".into(),
                payload: member_payload,
            },
        ];

        let affinity = AffinityIndex::new();
        process_pending_wal_records(&engine, &records, &affinity)
            .await
            .unwrap();

        // Verify affinity index has the group and member
        let gid = AffinityGroupId::from_string("g1");
        let group = affinity.get_group(&gid).expect("group should exist");
        assert!(
            group.members.contains(&LogicalId::from_string("e1")),
            "entity e1 should be a member of g1"
        );
        assert_eq!(
            affinity.group_for(&LogicalId::from_string("e1")),
            Some(gid),
        );
    }

    #[tokio::test]
    async fn process_pending_tier_migration_already_complete() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = make_engine(dir.path()).await;

        let entity_id = LogicalId::from_string("v1");

        // Pre-seed the target tier (NVMe/warm) with data so the migration
        // looks "already complete" — source absent, target present.
        let data = rmp_serde::to_vec_named(&vec![1.0_f32, 2.0, 3.0]).unwrap();
        engine
            .write_tiered("venues", &entity_id, Tier::NVMe, &data)
            .unwrap();

        // Build a TierMigration WAL record describing hot→warm
        let payload = TierMigrationPayload {
            entity_id: "v1".into(),
            collection: "venues".into(),
            from_tier: Tier::Fjall,
            to_tier: Tier::NVMe,
            encoding: Encoding::Int8,
        };
        let payload_bytes = rmp_serde::to_vec_named(&payload).unwrap();

        let records = vec![WalRecord {
            lsn: 10,
            ts: 0,
            tx_id: 10,
            record_type: RecordType::TierMigration,
            schema_ver: 1,
            collection: "venues".into(),
            payload: payload_bytes,
        }];

        let affinity = AffinityIndex::new();
        // Should not error — the AlreadyComplete branch tolerates missing
        // location table entries gracefully.
        process_pending_wal_records(&engine, &records, &affinity)
            .await
            .unwrap();

        // Target data should still be present
        let still_there = engine
            .read_tiered("venues", &entity_id, Tier::NVMe)
            .unwrap();
        assert!(still_there.is_some(), "target tier data should remain");
    }

    #[tokio::test]
    async fn process_pending_tier_migration_re_execute() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = make_engine(dir.path()).await;

        let entity_id = LogicalId::from_string("v2");

        // Pre-seed the source tier (Fjall/hot) — target absent → ReExecute
        let data = rmp_serde::to_vec_named(&vec![4.0_f32, 5.0, 6.0]).unwrap();
        engine
            .write_tiered("venues", &entity_id, Tier::Fjall, &data)
            .unwrap();

        let payload = TierMigrationPayload {
            entity_id: "v2".into(),
            collection: "venues".into(),
            from_tier: Tier::Fjall,
            to_tier: Tier::NVMe,
            encoding: Encoding::Int8,
        };
        let payload_bytes = rmp_serde::to_vec_named(&payload).unwrap();

        let records = vec![WalRecord {
            lsn: 20,
            ts: 0,
            tx_id: 20,
            record_type: RecordType::TierMigration,
            schema_ver: 1,
            collection: "venues".into(),
            payload: payload_bytes,
        }];

        let affinity = AffinityIndex::new();
        process_pending_wal_records(&engine, &records, &affinity)
            .await
            .unwrap();

        // Source should be deleted, target should exist
        let source = engine
            .read_tiered("venues", &entity_id, Tier::Fjall)
            .unwrap();
        assert!(source.is_none(), "source tier should be cleared after re-execution");

        let target = engine
            .read_tiered("venues", &entity_id, Tier::NVMe)
            .unwrap();
        assert!(target.is_some(), "target tier should have data after re-execution");
    }

    #[tokio::test]
    async fn process_pending_tier_migration_cleanup_source() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = make_engine(dir.path()).await;

        let entity_id = LogicalId::from_string("v3");

        // Both source and target exist → CleanupSource
        let data = rmp_serde::to_vec_named(&vec![7.0_f32, 8.0]).unwrap();
        engine
            .write_tiered("venues", &entity_id, Tier::Fjall, &data)
            .unwrap();
        engine
            .write_tiered("venues", &entity_id, Tier::NVMe, &data)
            .unwrap();

        let payload = TierMigrationPayload {
            entity_id: "v3".into(),
            collection: "venues".into(),
            from_tier: Tier::Fjall,
            to_tier: Tier::NVMe,
            encoding: Encoding::Int8,
        };
        let payload_bytes = rmp_serde::to_vec_named(&payload).unwrap();

        let records = vec![WalRecord {
            lsn: 30,
            ts: 0,
            tx_id: 30,
            record_type: RecordType::TierMigration,
            schema_ver: 1,
            collection: "venues".into(),
            payload: payload_bytes,
        }];

        let affinity = AffinityIndex::new();
        process_pending_wal_records(&engine, &records, &affinity)
            .await
            .unwrap();

        // Source should be deleted, target should remain
        let source = engine
            .read_tiered("venues", &entity_id, Tier::Fjall)
            .unwrap();
        assert!(source.is_none(), "source should be cleaned up");

        let target = engine
            .read_tiered("venues", &entity_id, Tier::NVMe)
            .unwrap();
        assert!(target.is_some(), "target should still exist");
    }
}
