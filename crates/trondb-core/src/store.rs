use std::path::Path;

use fjall::{Config, Keyspace, PartitionCreateOptions, PartitionHandle, PersistMode};

use crate::error::EngineError;
use crate::location::Tier;
use crate::types::{CollectionSchema, Entity, LogicalId};

const META_PARTITION: &str = "_meta";
const COLLECTION_PREFIX: &str = "collection:";
const ENTITY_PREFIX: &str = "entity:";
const EDGE_TYPE_PREFIX: &str = "edge_type:";
const EDGE_PREFIX: &str = "edge:";

pub struct FjallStore {
    keyspace: Keyspace,
    meta: PartitionHandle,
}

impl FjallStore {
    pub fn open(data_dir: &Path) -> Result<Self, EngineError> {
        let keyspace = Config::new(data_dir)
            .open()
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let meta = keyspace
            .open_partition(META_PARTITION, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        Ok(Self { keyspace, meta })
    }

    pub fn create_collection_schema(&self, schema: &CollectionSchema) -> Result<(), EngineError> {
        let key = format!("{COLLECTION_PREFIX}{}", schema.name);
        if self
            .meta
            .get(&key)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?
            .is_some()
        {
            return Err(EngineError::CollectionAlreadyExists(schema.name.clone()));
        }

        let bytes = rmp_serde::to_vec_named(schema)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        self.meta
            .insert(&key, bytes)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?;

        // Create the partition for entities in this collection
        self.keyspace
            .open_partition(&schema.name, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        // Create field index partitions for each declared index
        for idx in &schema.indexes {
            let partition_name = format!("{}.idx.{}", schema.name, idx.name);
            self.keyspace
                .open_partition(&partition_name, PartitionCreateOptions::default())
                .map_err(|e| EngineError::Storage(e.to_string()))?;
        }

        self.keyspace
            .persist(PersistMode::SyncAll)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        Ok(())
    }

    pub fn has_collection(&self, name: &str) -> bool {
        let key = format!("{COLLECTION_PREFIX}{name}");
        self.meta.get(&key).ok().flatten().is_some()
    }

    pub fn get_collection_schema(&self, name: &str) -> Result<CollectionSchema, EngineError> {
        let key = format!("{COLLECTION_PREFIX}{name}");
        let bytes = self
            .meta
            .get(&key)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?
            .ok_or_else(|| EngineError::CollectionNotFound(name.to_owned()))?;

        rmp_serde::from_slice(&bytes).map_err(|e| EngineError::Storage(e.to_string()))
    }

    pub fn list_collection_schemas(&self) -> Vec<CollectionSchema> {
        self.meta
            .prefix(COLLECTION_PREFIX)
            .filter_map(|kv| {
                let (_k, v) = kv.ok()?;
                rmp_serde::from_slice(&v).ok()
            })
            .collect()
    }

    pub fn list_collections(&self) -> Vec<String> {
        self.meta
            .prefix(COLLECTION_PREFIX)
            .filter_map(|kv| {
                let (k, _v) = kv.ok()?;
                let key = std::str::from_utf8(&k).ok()?;
                Some(key.strip_prefix(COLLECTION_PREFIX)?.to_owned())
            })
            .collect()
    }

    pub fn open_field_index_partition(
        &self,
        collection: &str,
        index_name: &str,
    ) -> Result<PartitionHandle, EngineError> {
        let partition_name = format!("{collection}.idx.{index_name}");
        self.keyspace
            .open_partition(&partition_name, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))
    }

    pub fn insert(&self, collection: &str, entity: Entity) -> Result<(), EngineError> {
        // Validate collection exists
        if !self.has_collection(collection) {
            return Err(EngineError::CollectionNotFound(collection.to_owned()));
        }

        let partition = self
            .keyspace
            .open_partition(collection, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let key = format!("{ENTITY_PREFIX}{}", entity.id);
        let bytes = rmp_serde::to_vec_named(&entity)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        partition
            .insert(&key, bytes)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?;

        Ok(())
    }

    pub fn get(&self, collection: &str, id: &LogicalId) -> Result<Entity, EngineError> {
        if !self.has_collection(collection) {
            return Err(EngineError::CollectionNotFound(collection.to_owned()));
        }

        let partition = self
            .keyspace
            .open_partition(collection, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let key = format!("{ENTITY_PREFIX}{id}");
        let bytes = partition
            .get(&key)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?
            .ok_or_else(|| EngineError::EntityNotFound(id.to_string()))?;

        rmp_serde::from_slice(&bytes).map_err(|e| EngineError::Storage(e.to_string()))
    }

    pub fn scan(&self, collection: &str) -> Result<Vec<Entity>, EngineError> {
        if !self.has_collection(collection) {
            return Err(EngineError::CollectionNotFound(collection.to_owned()));
        }

        let partition = self
            .keyspace
            .open_partition(collection, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let entities: Vec<Entity> = partition
            .prefix(ENTITY_PREFIX)
            .filter_map(|kv| {
                let (_k, v) = kv.ok()?;
                rmp_serde::from_slice(&v).ok()
            })
            .collect();

        Ok(entities)
    }

    // --- Edge Type methods ---

    pub fn create_edge_type(&self, edge_type: &crate::edge::EdgeType) -> Result<(), EngineError> {
        let key = format!("{EDGE_TYPE_PREFIX}{}", edge_type.name);
        if self.meta.get(&key)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?
            .is_some()
        {
            return Err(EngineError::EdgeTypeAlreadyExists(edge_type.name.clone()));
        }

        let bytes = rmp_serde::to_vec_named(edge_type)
            .map_err(|e| EngineError::Storage(e.to_string()))?;
        self.meta.insert(&key, bytes)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?;

        // Create partition for this edge type's edges
        let partition_name = format!("edges.{}", edge_type.name);
        self.keyspace
            .open_partition(&partition_name, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        self.keyspace.persist(PersistMode::SyncAll)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        Ok(())
    }

    pub fn get_edge_type(&self, name: &str) -> Result<crate::edge::EdgeType, EngineError> {
        let key = format!("{EDGE_TYPE_PREFIX}{name}");
        let bytes = self.meta.get(&key)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?
            .ok_or_else(|| EngineError::EdgeTypeNotFound(name.to_owned()))?;
        rmp_serde::from_slice(&bytes).map_err(|e| EngineError::Storage(e.to_string()))
    }

    pub fn has_edge_type(&self, name: &str) -> bool {
        let key = format!("{EDGE_TYPE_PREFIX}{name}");
        self.meta.get(&key).ok().flatten().is_some()
    }

    pub fn list_edge_types(&self) -> Vec<crate::edge::EdgeType> {
        self.meta
            .prefix(EDGE_TYPE_PREFIX)
            .filter_map(|kv| {
                let (_k, v) = kv.ok()?;
                rmp_serde::from_slice(&v).ok()
            })
            .collect()
    }

    // --- Edge methods ---

    pub fn insert_edge(&self, edge: &crate::edge::Edge) -> Result<(), EngineError> {
        let partition_name = format!("edges.{}", edge.edge_type);
        let partition = self.keyspace
            .open_partition(&partition_name, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let key = format!("{EDGE_PREFIX}{}:{}", edge.from_id, edge.to_id);
        let bytes = rmp_serde::to_vec_named(edge)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        partition.insert(&key, bytes)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn get_edge(&self, edge_type: &str, from_id: &str, to_id: &str) -> Result<Option<crate::edge::Edge>, EngineError> {
        let partition_name = format!("edges.{edge_type}");
        let partition = self.keyspace
            .open_partition(&partition_name, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let key = format!("{EDGE_PREFIX}{from_id}:{to_id}");
        match partition.get(&key).map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))? {
            Some(bytes) => {
                let edge: crate::edge::Edge = rmp_serde::from_slice(&bytes)
                    .map_err(|e| EngineError::Storage(e.to_string()))?;
                Ok(Some(edge))
            }
            None => Ok(None),
        }
    }

    pub fn delete_edge(&self, edge_type: &str, from_id: &str, to_id: &str) -> Result<(), EngineError> {
        let partition_name = format!("edges.{edge_type}");
        let partition = self.keyspace
            .open_partition(&partition_name, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let key = format!("{EDGE_PREFIX}{from_id}:{to_id}");
        partition.remove(&key)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn scan_edges(&self, edge_type: &str) -> Result<Vec<crate::edge::Edge>, EngineError> {
        let partition_name = format!("edges.{edge_type}");
        let partition = self.keyspace
            .open_partition(&partition_name, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let edges: Vec<crate::edge::Edge> = partition
            .prefix(EDGE_PREFIX)
            .filter_map(|kv| {
                let (_k, v) = kv.ok()?;
                rmp_serde::from_slice(&v).ok()
            })
            .collect();

        Ok(edges)
    }

    pub fn delete_entity(&self, collection: &str, id: &LogicalId) -> Result<(), EngineError> {
        if !self.has_collection(collection) {
            return Err(EngineError::CollectionNotFound(collection.to_owned()));
        }

        let partition = self
            .keyspace
            .open_partition(collection, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let key = format!("{ENTITY_PREFIX}{id}");
        partition
            .remove(&key)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?;

        Ok(())
    }

    pub fn persist(&self) -> Result<(), EngineError> {
        self.keyspace
            .persist(PersistMode::SyncAll)
            .map_err(|e| EngineError::Storage(e.to_string()))
    }

    /// Returns the total number of entities across all collections.
    pub fn entity_count(&self) -> usize {
        let mut count = 0;
        for name in self.list_collections() {
            if let Ok(partition) = self
                .keyspace
                .open_partition(&name, PartitionCreateOptions::default())
            {
                count += partition.prefix(ENTITY_PREFIX).count();
            }
        }
        count
    }

    /// Get the Fjall partition name for a given collection and tier.
    pub fn tier_partition_name(collection: &str, tier: Tier) -> String {
        match tier {
            Tier::Fjall | Tier::Ram => collection.to_string(),
            Tier::NVMe => format!("warm.{collection}"),
            Tier::Archive => format!("archive.{collection}"),
        }
    }

    /// Read entity data from a specific tier's partition.
    pub fn read_tiered(
        &self,
        collection: &str,
        entity_id: &LogicalId,
        tier: Tier,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        let part_name = Self::tier_partition_name(collection, tier);
        let partition = self
            .keyspace
            .open_partition(&part_name, Default::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let key = format!("entity:{}", entity_id.as_str());
        match partition.get(key.as_bytes()) {
            Ok(Some(val)) => Ok(Some(val.to_vec())),
            Ok(None) => Ok(None),
            Err(e) => Err(EngineError::Storage(e.to_string())),
        }
    }

    /// Write entity data to a specific tier's partition.
    pub fn write_tiered(
        &self,
        collection: &str,
        entity_id: &LogicalId,
        tier: Tier,
        data: &[u8],
    ) -> Result<(), EngineError> {
        let part_name = Self::tier_partition_name(collection, tier);
        let partition = self
            .keyspace
            .open_partition(&part_name, Default::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let key = format!("entity:{}", entity_id.as_str());
        partition
            .insert(key.as_bytes(), data)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        self.keyspace
            .persist(fjall::PersistMode::SyncAll)
            .map_err(|e| EngineError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Delete entity from a specific tier's partition.
    pub fn delete_from_tier(
        &self,
        collection: &str,
        entity_id: &LogicalId,
        tier: Tier,
    ) -> Result<(), EngineError> {
        let part_name = Self::tier_partition_name(collection, tier);
        let partition = self
            .keyspace
            .open_partition(&part_name, Default::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let key = format!("entity:{}", entity_id.as_str());
        partition
            .remove(key.as_bytes())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        self.keyspace
            .persist(fjall::PersistMode::SyncAll)
            .map_err(|e| EngineError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Count entities in a specific tier's partition for a collection.
    pub fn tier_entity_count(
        &self,
        collection: &str,
        tier: Tier,
    ) -> Result<usize, EngineError> {
        let part_name = Self::tier_partition_name(collection, tier);
        let partition = self
            .keyspace
            .open_partition(&part_name, Default::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        let prefix = b"entity:";
        let count = partition.prefix(prefix).count();
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CollectionSchema, Metric, StoredRepresentation, Value};
    use tempfile::TempDir;

    fn open_store() -> (FjallStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        (store, dir)
    }

    fn make_schema(name: &str, dims: usize) -> CollectionSchema {
        CollectionSchema {
            name: name.into(),
            representations: vec![StoredRepresentation {
                name: "default".into(),
                model: None,
                dimensions: Some(dims),
                metric: Metric::Cosine,
                sparse: false,
            fields: vec![],
            }],
            fields: vec![],
            indexes: vec![],
            vectoriser_config: None,
        }
    }

    #[test]
    fn create_collection() {
        let (store, _dir) = open_store();
        let schema = make_schema("docs", 384);
        store.create_collection_schema(&schema).unwrap();
        assert!(store.has_collection("docs"));
    }

    #[test]
    fn create_duplicate_collection_fails() {
        let (store, _dir) = open_store();
        let schema = make_schema("docs", 384);
        store.create_collection_schema(&schema).unwrap();
        assert!(store.create_collection_schema(&schema).is_err());
    }

    #[test]
    fn insert_and_get_entity() {
        let (store, _dir) = open_store();
        let schema = make_schema("docs", 3);
        store.create_collection_schema(&schema).unwrap();

        let id = LogicalId::from_string("e1");
        let entity = Entity::new(id.clone()).with_metadata("title", Value::String("Hello".into()));

        store.insert("docs", entity).unwrap();

        let retrieved = store.get("docs", &id).unwrap();
        assert_eq!(
            retrieved.metadata.get("title"),
            Some(&Value::String("Hello".into()))
        );
    }

    #[test]
    fn insert_into_nonexistent_collection_fails() {
        let (store, _dir) = open_store();
        let entity = Entity::new(LogicalId::new());
        assert!(store.insert("nope", entity).is_err());
    }

    #[test]
    fn scan_all_entities() {
        let (store, _dir) = open_store();
        let schema = make_schema("docs", 3);
        store.create_collection_schema(&schema).unwrap();

        for i in 0..5 {
            let entity = Entity::new(LogicalId::from_string(&format!("e{i}")));
            store.insert("docs", entity).unwrap();
        }

        let all = store.scan("docs").unwrap();
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn get_collection_schema_round_trip() {
        let (store, _dir) = open_store();
        let schema = make_schema("docs", 1408);
        store.create_collection_schema(&schema).unwrap();
        let retrieved = store.get_collection_schema("docs").unwrap();
        assert_eq!(retrieved.name, "docs");
        assert_eq!(retrieved.representations[0].dimensions, Some(1408));
    }

    #[test]
    fn delete_entity_removes_from_fjall() {
        let (store, _dir) = open_store();
        let schema = make_schema("venues", 3);
        store.create_collection_schema(&schema).unwrap();

        let entity = Entity::new(LogicalId::from_string("e1"))
            .with_metadata("name", Value::String("Test".into()));
        store.insert("venues", entity).unwrap();
        store.persist().unwrap();

        store.delete_entity("venues", &LogicalId::from_string("e1")).unwrap();
        store.persist().unwrap();

        assert!(store.get("venues", &LogicalId::from_string("e1")).is_err());
    }

    #[test]
    fn data_survives_reopen() {
        let dir = TempDir::new().unwrap();

        // Write
        {
            let store = FjallStore::open(dir.path()).unwrap();
            let schema = make_schema("venues", 3);
            store.create_collection_schema(&schema).unwrap();
            let entity = Entity::new(LogicalId::from_string("v1"))
                .with_metadata("name", Value::String("The Shard".into()));
            store.insert("venues", entity).unwrap();
        }

        // Reopen and read
        {
            let store = FjallStore::open(dir.path()).unwrap();
            assert!(store.has_collection("venues"));
            let entity = store
                .get("venues", &LogicalId::from_string("v1"))
                .unwrap();
            assert_eq!(
                entity.metadata.get("name"),
                Some(&Value::String("The Shard".into()))
            );
        }
    }
}

#[cfg(test)]
mod tier_tests {
    use super::*;
    use crate::location::Tier;
    use crate::types::LogicalId;

    fn temp_store() -> (FjallStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        (store, dir)
    }

    #[test]
    fn write_and_read_warm_tier() {
        let (store, _dir) = temp_store();
        let id = LogicalId::from_string("e1");
        let data = b"int8 quantised data";

        store.write_tiered("venues", &id, Tier::NVMe, data).unwrap();
        let result = store.read_tiered("venues", &id, Tier::NVMe).unwrap();
        assert_eq!(result.as_deref(), Some(data.as_slice()));
    }

    #[test]
    fn write_and_read_archive_tier() {
        let (store, _dir) = temp_store();
        let id = LogicalId::from_string("e1");
        let data = b"binary quantised data";

        store.write_tiered("venues", &id, Tier::Archive, data).unwrap();
        let result = store.read_tiered("venues", &id, Tier::Archive).unwrap();
        assert_eq!(result.as_deref(), Some(data.as_slice()));
    }

    #[test]
    fn read_nonexistent_returns_none() {
        let (store, _dir) = temp_store();
        let id = LogicalId::from_string("nope");
        let result = store.read_tiered("venues", &id, Tier::NVMe).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn delete_from_tier() {
        let (store, _dir) = temp_store();
        let id = LogicalId::from_string("e1");
        store.write_tiered("venues", &id, Tier::NVMe, b"data").unwrap();
        store.delete_from_tier("venues", &id, Tier::NVMe).unwrap();
        let result = store.read_tiered("venues", &id, Tier::NVMe).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn tier_entity_count() {
        let (store, _dir) = temp_store();
        let id1 = LogicalId::from_string("e1");
        let id2 = LogicalId::from_string("e2");
        store.write_tiered("venues", &id1, Tier::NVMe, b"data1").unwrap();
        store.write_tiered("venues", &id2, Tier::NVMe, b"data2").unwrap();
        assert_eq!(store.tier_entity_count("venues", Tier::NVMe).unwrap(), 2);
        assert_eq!(store.tier_entity_count("venues", Tier::Archive).unwrap(), 0);
    }

    #[test]
    fn tier_partition_names() {
        assert_eq!(FjallStore::tier_partition_name("venues", Tier::Fjall), "venues");
        assert_eq!(FjallStore::tier_partition_name("venues", Tier::NVMe), "warm.venues");
        assert_eq!(FjallStore::tier_partition_name("venues", Tier::Archive), "archive.venues");
    }
}
