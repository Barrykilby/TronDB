use std::path::Path;

use fjall::{Config, Keyspace, PartitionCreateOptions, PartitionHandle, PersistMode};

use crate::error::EngineError;
use crate::types::{Collection, Entity, LogicalId};

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

    pub fn create_collection(&self, name: &str, dimensions: usize) -> Result<(), EngineError> {
        let key = format!("{COLLECTION_PREFIX}{name}");
        if self
            .meta
            .get(&key)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?
            .is_some()
        {
            return Err(EngineError::CollectionAlreadyExists(name.to_owned()));
        }

        let collection = Collection::new(name, dimensions)?;
        let bytes = rmp_serde::to_vec_named(&collection)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        self.meta
            .insert(&key, bytes)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?;

        // Create the partition for entities in this collection
        self.keyspace
            .open_partition(name, PartitionCreateOptions::default())
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        self.keyspace
            .persist(PersistMode::SyncAll)
            .map_err(|e| EngineError::Storage(e.to_string()))?;

        Ok(())
    }

    pub fn has_collection(&self, name: &str) -> bool {
        let key = format!("{COLLECTION_PREFIX}{name}");
        self.meta.get(&key).ok().flatten().is_some()
    }

    pub fn get_dimensions(&self, collection: &str) -> Result<usize, EngineError> {
        let col = self.get_collection_meta(collection)?;
        Ok(col.dimensions)
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

    pub fn insert(&self, collection: &str, entity: Entity) -> Result<(), EngineError> {
        // Validate collection exists and check dimensions
        let col = self.get_collection_meta(collection)?;
        for repr in &entity.representations {
            if repr.vector.len() != col.dimensions {
                return Err(EngineError::DimensionMismatch {
                    expected: col.dimensions,
                    got: repr.vector.len(),
                });
            }
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

    pub fn persist(&self) -> Result<(), EngineError> {
        self.keyspace
            .persist(PersistMode::SyncAll)
            .map_err(|e| EngineError::Storage(e.to_string()))
    }

    fn get_collection_meta(&self, name: &str) -> Result<Collection, EngineError> {
        let key = format!("{COLLECTION_PREFIX}{name}");
        let bytes = self
            .meta
            .get(&key)
            .map_err(|e: fjall::Error| EngineError::Storage(e.to_string()))?
            .ok_or_else(|| EngineError::CollectionNotFound(name.to_owned()))?;

        rmp_serde::from_slice(&bytes).map_err(|e| EngineError::Storage(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;
    use tempfile::TempDir;

    fn open_store() -> (FjallStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = FjallStore::open(dir.path()).unwrap();
        (store, dir)
    }

    #[test]
    fn create_collection() {
        let (store, _dir) = open_store();
        store.create_collection("docs", 384).unwrap();
        assert!(store.has_collection("docs"));
    }

    #[test]
    fn create_duplicate_collection_fails() {
        let (store, _dir) = open_store();
        store.create_collection("docs", 384).unwrap();
        assert!(store.create_collection("docs", 384).is_err());
    }

    #[test]
    fn insert_and_get_entity() {
        let (store, _dir) = open_store();
        store.create_collection("docs", 3).unwrap();

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
        store.create_collection("docs", 3).unwrap();

        for i in 0..5 {
            let entity = Entity::new(LogicalId::from_string(&format!("e{i}")));
            store.insert("docs", entity).unwrap();
        }

        let all = store.scan("docs").unwrap();
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn get_collection_dimensions() {
        let (store, _dir) = open_store();
        store.create_collection("docs", 1408).unwrap();
        assert_eq!(store.get_dimensions("docs").unwrap(), 1408);
    }

    #[test]
    fn data_survives_reopen() {
        let dir = TempDir::new().unwrap();

        // Write
        {
            let store = FjallStore::open(dir.path()).unwrap();
            store.create_collection("venues", 3).unwrap();
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
