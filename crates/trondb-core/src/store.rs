use dashmap::DashMap;

use crate::error::EngineError;
use crate::types::{Collection, Entity, LogicalId};

struct CollectionStore {
    collection: Collection,
    entities: DashMap<LogicalId, Entity>,
}

pub struct Store {
    collections: DashMap<String, CollectionStore>,
}

impl Store {
    pub fn new() -> Self {
        Self {
            collections: DashMap::new(),
        }
    }

    pub fn create_collection(&self, name: &str, dimensions: usize) -> Result<(), EngineError> {
        if self.collections.contains_key(name) {
            return Err(EngineError::CollectionAlreadyExists(name.to_owned()));
        }
        let collection = Collection::new(name, dimensions)?;
        self.collections.insert(
            name.to_owned(),
            CollectionStore {
                collection,
                entities: DashMap::new(),
            },
        );
        Ok(())
    }

    pub fn has_collection(&self, name: &str) -> bool {
        self.collections.contains_key(name)
    }

    pub fn get_dimensions(&self, collection: &str) -> Result<usize, EngineError> {
        self.collections
            .get(collection)
            .map(|cs| cs.collection.dimensions)
            .ok_or_else(|| EngineError::CollectionNotFound(collection.to_owned()))
    }

    pub fn list_collections(&self) -> Vec<String> {
        self.collections.iter().map(|r| r.key().clone()).collect()
    }

    pub fn insert(&self, collection: &str, entity: Entity) -> Result<(), EngineError> {
        let cs = self
            .collections
            .get(collection)
            .ok_or_else(|| EngineError::CollectionNotFound(collection.to_owned()))?;

        // Validate vector dimensions on all representations
        let expected = cs.collection.dimensions;
        for repr in &entity.representations {
            if repr.vector.len() != expected {
                return Err(EngineError::DimensionMismatch {
                    expected,
                    got: repr.vector.len(),
                });
            }
        }

        cs.entities.insert(entity.id.clone(), entity);
        Ok(())
    }

    pub fn get(&self, collection: &str, id: &LogicalId) -> Result<Entity, EngineError> {
        let cs = self
            .collections
            .get(collection)
            .ok_or_else(|| EngineError::CollectionNotFound(collection.to_owned()))?;

        cs.entities
            .get(id)
            .map(|e| e.value().clone())
            .ok_or_else(|| EngineError::EntityNotFound(id.to_string()))
    }

    pub fn scan(&self, collection: &str) -> Result<Vec<Entity>, EngineError> {
        let cs = self
            .collections
            .get(collection)
            .ok_or_else(|| EngineError::CollectionNotFound(collection.to_owned()))?;

        Ok(cs.entities.iter().map(|r| r.value().clone()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;

    #[test]
    fn create_collection() {
        let store = Store::new();
        store.create_collection("docs", 384).unwrap();
        assert!(store.has_collection("docs"));
    }

    #[test]
    fn create_duplicate_collection_fails() {
        let store = Store::new();
        store.create_collection("docs", 384).unwrap();
        assert!(store.create_collection("docs", 384).is_err());
    }

    #[test]
    fn insert_and_get_entity() {
        let store = Store::new();
        store.create_collection("docs", 3).unwrap();

        let id = LogicalId::from_str("e1");
        let entity = Entity::new(id.clone())
            .with_metadata("title", Value::String("Hello".into()));

        store.insert("docs", entity).unwrap();

        let retrieved = store.get("docs", &id).unwrap();
        assert_eq!(
            retrieved.metadata.get("title"),
            Some(&Value::String("Hello".into()))
        );
    }

    #[test]
    fn insert_into_nonexistent_collection_fails() {
        let store = Store::new();
        let entity = Entity::new(LogicalId::new());
        assert!(store.insert("nope", entity).is_err());
    }

    #[test]
    fn scan_all_entities() {
        let store = Store::new();
        store.create_collection("docs", 3).unwrap();

        for i in 0..5 {
            let entity = Entity::new(LogicalId::from_str(&format!("e{i}")));
            store.insert("docs", entity).unwrap();
        }

        let all = store.scan("docs").unwrap();
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn get_collection_dimensions() {
        let store = Store::new();
        store.create_collection("docs", 1408).unwrap();
        assert_eq!(store.get_dimensions("docs").unwrap(), 1408);
    }
}
