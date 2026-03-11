use std::sync::Arc;
use std::time::SystemTime;

use trondb_core::location::{Encoding, LocState, ReprKey, Tier};
use trondb_core::quantise::{quantise_binary, quantise_int8, dequantise_int8, QuantisedInt8};
use trondb_core::Engine;
use trondb_wal::record::RecordType;

use crate::config::TierConfig;
use crate::error::RouterError;
use crate::eviction::{select_eviction_candidates, LruTracker};
use crate::affinity::AffinityIndex;
use crate::node::EntityId;

/// Payload for TierMigration WAL records.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct TierMigrationPayload {
    pub entity_id: String,
    pub collection: String,
    pub from_tier: Tier,
    pub to_tier: Tier,
    pub encoding: Encoding,
}

pub struct TierMigrator {
    config: TierConfig,
    pub(crate) engine: Arc<Engine>,
    lru: Arc<std::sync::Mutex<LruTracker>>,
    affinity: Arc<AffinityIndex>,
}

impl TierMigrator {
    pub fn new(
        config: TierConfig,
        engine: Arc<Engine>,
        lru: Arc<std::sync::Mutex<LruTracker>>,
        affinity: Arc<AffinityIndex>,
    ) -> Self {
        Self { config, engine, lru, affinity }
    }

    /// Run one demotion cycle: hot → warm for idle entities.
    pub async fn demote_hot_to_warm(&self, collection: &str) -> Result<usize, RouterError> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let threshold = now.saturating_sub(self.config.demote_after_secs);

        let location = self.engine.location();
        let mut candidates: Vec<EntityId> = Vec::new();

        for entry in location.iter() {
            let (key, desc) = entry.pair();
            if desc.tier == Tier::Fjall
                && desc.state == LocState::Clean
                && desc.last_accessed < threshold
            {
                candidates.push(key.entity_id.clone());
            }
        }

        if candidates.is_empty() {
            return Ok(0);
        }

        let lru_guard = self.lru.lock().unwrap();
        let sorted = select_eviction_candidates(
            &candidates,
            candidates.len().min(self.config.migration_batch_size),
            &self.affinity,
            &lru_guard,
        );
        drop(lru_guard);

        let mut migrated = 0;
        for entity_id in sorted.iter().take(self.config.migration_batch_size) {
            match self.demote_single(collection, entity_id, Tier::Fjall, Tier::NVMe).await {
                Ok(()) => migrated += 1,
                Err(e) => {
                    eprintln!("[migrator] demotion failed for {}: {e}", entity_id.as_str());
                }
            }
        }
        Ok(migrated)
    }

    /// Run one demotion cycle: warm → archive for idle entities.
    pub async fn demote_warm_to_archive(&self, collection: &str) -> Result<usize, RouterError> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let threshold = now.saturating_sub(self.config.archive_after_secs);

        let location = self.engine.location();
        let mut candidates: Vec<EntityId> = Vec::new();

        for entry in location.iter() {
            let (key, desc) = entry.pair();
            if desc.tier == Tier::NVMe
                && desc.state == LocState::Clean
                && desc.last_accessed < threshold
            {
                candidates.push(key.entity_id.clone());
            }
        }

        let mut migrated = 0;
        for entity_id in candidates.iter().take(self.config.migration_batch_size) {
            match self.demote_single(collection, entity_id, Tier::NVMe, Tier::Archive).await {
                Ok(()) => migrated += 1,
                Err(e) => {
                    eprintln!(
                        "[migrator] warm->archive demotion failed for {}: {e}",
                        entity_id.as_str()
                    );
                }
            }
        }
        Ok(migrated)
    }

    pub(crate) async fn demote_single(
        &self,
        collection: &str,
        entity_id: &EntityId,
        from_tier: Tier,
        to_tier: Tier,
    ) -> Result<(), RouterError> {
        let location = self.engine.location();
        let repr_key = ReprKey {
            entity_id: entity_id.clone(),
            repr_index: 0,
        };

        // Step a: Mark as migrating
        location
            .transition(&repr_key, LocState::Migrating)
            .map_err(RouterError::Engine)?;

        // Step b: WAL log intent
        let wal = self.engine.wal_writer();
        let target_encoding = match to_tier {
            Tier::NVMe => Encoding::Int8,
            Tier::Archive => Encoding::Binary,
            _ => Encoding::Float32,
        };
        let payload = rmp_serde::to_vec_named(&TierMigrationPayload {
            entity_id: entity_id.as_str().to_owned(),
            collection: collection.to_owned(),
            from_tier,
            to_tier,
            encoding: target_encoding,
        })
        .map_err(|e| {
            RouterError::Engine(trondb_core::error::EngineError::Storage(e.to_string()))
        })?;

        let tx_id = wal.next_tx_id();
        wal.append(RecordType::TierMigration, collection, tx_id, 0, payload);
        wal.commit(tx_id)
            .await
            .map_err(|e| {
                RouterError::Engine(trondb_core::error::EngineError::Storage(e.to_string()))
            })?;

        // Step c: Read from source tier
        let data = self
            .engine
            .read_tiered(collection, entity_id, from_tier)
            .map_err(RouterError::Engine)?
            .ok_or_else(|| {
                RouterError::Engine(trondb_core::error::EngineError::EntityNotFound(
                    entity_id.as_str().to_owned(),
                ))
            })?;

        // Step d: Transcode and write to target tier
        match to_tier {
            Tier::NVMe => {
                let vector: Vec<f32> = rmp_serde::from_slice(&data).map_err(|e| {
                    RouterError::Engine(trondb_core::error::EngineError::Storage(e.to_string()))
                })?;
                let quantised = quantise_int8(&vector);
                self.engine
                    .write_tiered(collection, entity_id, Tier::NVMe, &quantised.to_bytes())
                    .map_err(RouterError::Engine)?;
            }
            Tier::Archive => {
                let int8 = QuantisedInt8::from_bytes(&data).map_err(RouterError::Engine)?;
                let float_vec = dequantise_int8(&int8);
                let binary = quantise_binary(&float_vec);
                self.engine
                    .write_tiered(collection, entity_id, Tier::Archive, &binary.to_bytes())
                    .map_err(RouterError::Engine)?;
            }
            _ => {}
        }

        // Step e: Remove from HNSW (only hot -> warm)
        if from_tier == Tier::Fjall {
            self.engine.remove_from_hnsw(collection, entity_id);
        }

        // Step f: Update location table
        let (target_enc, target_t) = match to_tier {
            Tier::NVMe => (Encoding::Int8, Tier::NVMe),
            Tier::Archive => (Encoding::Binary, Tier::Archive),
            _ => (Encoding::Float32, to_tier),
        };
        location
            .update_tier(&repr_key, target_t, target_enc)
            .map_err(RouterError::Engine)?;

        // Step g: Complete migration
        location
            .transition(&repr_key, LocState::Clean)
            .map_err(RouterError::Engine)?;

        // Step h: Delete from source
        self.engine
            .delete_from_tier(collection, entity_id, from_tier)
            .map_err(RouterError::Engine)?;

        Ok(())
    }

    /// Promote a warm-tier entity back to hot.
    pub async fn promote_to_hot(
        &self,
        collection: &str,
        entity_id: &EntityId,
        vector: Vec<f32>,
    ) -> Result<(), RouterError> {
        let location = self.engine.location();
        let repr_key = ReprKey {
            entity_id: entity_id.clone(),
            repr_index: 0,
        };

        // Write Float32 to hot partition
        let data = rmp_serde::to_vec_named(&vector).map_err(|e| {
            RouterError::Engine(trondb_core::error::EngineError::Storage(e.to_string()))
        })?;
        self.engine
            .write_tiered(collection, entity_id, Tier::Fjall, &data)
            .map_err(RouterError::Engine)?;

        // Insert into HNSW
        self.engine
            .insert_into_hnsw(collection, entity_id, &vector);

        // WAL log
        let wal = self.engine.wal_writer();
        let payload = rmp_serde::to_vec_named(&TierMigrationPayload {
            entity_id: entity_id.as_str().to_owned(),
            collection: collection.to_owned(),
            from_tier: Tier::NVMe,
            to_tier: Tier::Fjall,
            encoding: Encoding::Float32,
        })
        .map_err(|e| {
            RouterError::Engine(trondb_core::error::EngineError::Storage(e.to_string()))
        })?;
        let tx_id = wal.next_tx_id();
        wal.append(RecordType::TierMigration, collection, tx_id, 0, payload);
        wal.commit(tx_id)
            .await
            .map_err(|e| {
                RouterError::Engine(trondb_core::error::EngineError::Storage(e.to_string()))
            })?;

        // Update location table
        location
            .update_tier(&repr_key, Tier::Fjall, Encoding::Float32)
            .map_err(RouterError::Engine)?;

        // Delete from warm
        self.engine
            .delete_from_tier(collection, entity_id, Tier::NVMe)
            .map_err(RouterError::Engine)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_migration_payload_serialisation() {
        let payload = TierMigrationPayload {
            entity_id: "e1".to_owned(),
            collection: "venues".to_owned(),
            from_tier: Tier::Fjall,
            to_tier: Tier::NVMe,
            encoding: Encoding::Int8,
        };
        let bytes = rmp_serde::to_vec_named(&payload).unwrap();
        let restored: TierMigrationPayload = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(restored.entity_id, "e1");
        assert_eq!(restored.to_tier, Tier::NVMe);
    }
}
