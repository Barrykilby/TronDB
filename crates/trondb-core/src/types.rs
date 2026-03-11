use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use uuid::Uuid;

use crate::error::EngineError;

// ---------------------------------------------------------------------------
// LogicalId
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LogicalId(String);

impl LogicalId {
    /// Generate a new random logical ID (UUID v4).
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Create a logical ID from an existing string.
    pub fn from_string(s: &str) -> Self {
        Self(s.to_owned())
    }

    /// Borrow the inner string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for LogicalId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for LogicalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Value
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Value {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::String(s) => write!(f, "{s}"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(v) => write!(f, "{v}"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Null => write!(f, "NULL"),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Null, Value::Null) => true,
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// ReprType / ReprState / Representation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReprType {
    Atomic,
    Composite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReprState {
    Clean,
    Dirty,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Representation {
    pub repr_type: ReprType,
    pub fields: Vec<String>,
    pub vector: Vec<f32>,
    pub recipe_hash: [u8; 32],
    pub state: ReprState,
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: LogicalId,
    #[serde(skip)]
    pub raw_data: Bytes,
    pub metadata: HashMap<String, Value>,
    pub representations: Vec<Representation>,
    pub schema_version: u32,
}

impl Entity {
    /// Create a new entity with the given ID and sensible defaults.
    pub fn new(id: LogicalId) -> Self {
        Self {
            id,
            raw_data: Bytes::new(),
            metadata: HashMap::new(),
            representations: Vec::new(),
            schema_version: 1,
        }
    }

    /// Add a metadata key-value pair (builder style).
    pub fn with_metadata(mut self, key: impl Into<String>, value: Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }

    /// Add a representation (builder style).
    pub fn with_representation(mut self, repr: Representation) -> Self {
        self.representations.push(repr);
        self
    }

    /// Set raw data (builder style).
    pub fn with_raw_data(mut self, data: Bytes) -> Self {
        self.raw_data = data;
        self
    }
}

// ---------------------------------------------------------------------------
// VectorData — dense or sparse vector payload
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VectorData {
    Dense(Vec<f32>),
    Sparse(Vec<(u32, f32)>),
}

// ---------------------------------------------------------------------------
// Schema types — Phase 5a collection schema
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Metric {
    Cosine,
    InnerProduct,
}

impl Default for Metric {
    fn default() -> Self {
        Metric::Cosine
    }
}

/// Field type for schema declarations. Uses the canonical name `FieldType`
/// from the spec. The old `FieldType` in types.rs is renamed to `LegacyFieldType`
/// in this same task to avoid collision. `LegacyFieldType` is removed in Task 8.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FieldType {
    Text,
    DateTime,
    Bool,
    Int,
    Float,
    EntityRef(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionSchema {
    pub name: String,
    pub representations: Vec<StoredRepresentation>,
    pub fields: Vec<StoredField>,
    pub indexes: Vec<StoredIndex>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredRepresentation {
    pub name: String,
    pub model: Option<String>,
    pub dimensions: Option<usize>,
    pub metric: Metric,
    pub sparse: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredField {
    pub name: String,
    pub field_type: FieldType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredIndex {
    pub name: String,
    pub fields: Vec<String>,
    pub partial_condition: Option<String>,
}

// ---------------------------------------------------------------------------
// LegacyFieldType / FieldDef / Collection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LegacyFieldType {
    String,
    Int,
    Float,
    Bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: String,
    pub field_type: LegacyFieldType,
    pub indexed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub name: String,
    pub dimensions: usize,
    pub fields: Vec<FieldDef>,
}

impl Collection {
    /// Create a new collection. Fails if `dims` is zero.
    pub fn new(name: impl Into<String>, dims: usize) -> Result<Self, EngineError> {
        if dims == 0 {
            return Err(EngineError::DimensionMismatch {
                expected: 1,
                got: 0,
            });
        }
        Ok(Self {
            name: name.into(),
            dimensions: dims,
            fields: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_id_is_unique() {
        let a = LogicalId::new();
        let b = LogicalId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn logical_id_from_string() {
        let id = LogicalId::from_string("test-id-1");
        assert_eq!(id.as_str(), "test-id-1");
    }

    #[test]
    fn value_display() {
        assert_eq!(Value::String("hello".into()).to_string(), "hello");
        assert_eq!(Value::Int(42).to_string(), "42");
        assert_eq!(Value::Float(3.14).to_string(), "3.14");
        assert_eq!(Value::Bool(true).to_string(), "true");
        assert_eq!(Value::Null.to_string(), "NULL");
    }

    #[test]
    fn entity_builder() {
        let entity = Entity::new(LogicalId::from_string("e1"))
            .with_metadata("name", Value::String("Alice".into()))
            .with_metadata("age", Value::Int(30));

        assert_eq!(
            entity.metadata.get("name"),
            Some(&Value::String("Alice".into()))
        );
        assert_eq!(entity.metadata.get("age"), Some(&Value::Int(30)));
        assert!(entity.representations.is_empty());
    }

    #[test]
    fn collection_validates_dimensions() {
        assert!(Collection::new("test", 0).is_err());
        let col = Collection::new("test", 384).unwrap();
        assert_eq!(col.dimensions, 384);
    }

    #[test]
    fn vector_data_dense() {
        let v = VectorData::Dense(vec![1.0, 2.0, 3.0]);
        match &v {
            VectorData::Dense(d) => assert_eq!(d.len(), 3),
            _ => panic!("expected Dense"),
        }
    }

    #[test]
    fn vector_data_sparse() {
        let v = VectorData::Sparse(vec![(1, 0.8), (42, 0.5)]);
        match &v {
            VectorData::Sparse(s) => {
                assert_eq!(s.len(), 2);
                assert_eq!(s[0].0, 1);
            }
            _ => panic!("expected Sparse"),
        }
    }

    #[test]
    fn metric_default_is_cosine() {
        let m = Metric::default();
        assert_eq!(m, Metric::Cosine);
    }

    #[test]
    fn collection_schema_round_trip() {
        let schema = CollectionSchema {
            name: "test".into(),
            representations: vec![StoredRepresentation {
                name: "identity".into(),
                model: Some("jina-v4".into()),
                dimensions: Some(1024),
                metric: Metric::Cosine,
                sparse: false,
            }],
            fields: vec![StoredField {
                name: "status".into(),
                field_type: FieldType::Text,
            }],
            indexes: vec![StoredIndex {
                name: "idx_status".into(),
                fields: vec!["status".into()],
                partial_condition: None,
            }],
        };
        let bytes = rmp_serde::to_vec_named(&schema).unwrap();
        let restored: CollectionSchema = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(restored.name, "test");
        assert_eq!(restored.representations.len(), 1);
        assert_eq!(restored.fields.len(), 1);
        assert_eq!(restored.indexes.len(), 1);
    }

    #[test]
    fn schema_field_type_variants() {
        assert_ne!(FieldType::Text, FieldType::Int);
        assert_ne!(FieldType::DateTime, FieldType::Bool);
        assert_eq!(
            FieldType::EntityRef("venues".into()),
            FieldType::EntityRef("venues".into())
        );
    }
}
