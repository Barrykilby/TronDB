use crate::error::EngineError;
use crate::types::{FieldType, LogicalId, Value};

// ---------------------------------------------------------------------------
// Sortable byte encoding
// ---------------------------------------------------------------------------

/// Encode a Value as sortable bytes for field index keys.
pub fn encode_sortable(value: &Value, field_type: &FieldType) -> Result<Vec<u8>, EngineError> {
    match (value, field_type) {
        (Value::String(s), FieldType::Text | FieldType::EntityRef(_) | FieldType::DateTime) => {
            let mut bytes = s.as_bytes().to_vec();
            bytes.push(0x00); // null terminator for compound key separation
            Ok(bytes)
        }
        (Value::Bool(b), FieldType::Bool) => {
            Ok(vec![if *b { 0x01 } else { 0x00 }])
        }
        (Value::Int(n), FieldType::Int) => {
            // Big-endian i64 with sign bit flipped so negative sorts before positive
            let flipped = (*n as u64) ^ (1u64 << 63);
            Ok(flipped.to_be_bytes().to_vec())
        }
        (Value::Float(f), FieldType::Float) => {
            // IEEE 754 manipulation for sort order
            let bits = f.to_bits();
            let sortable = if *f >= 0.0 {
                bits ^ (1u64 << 63) // flip sign bit for positive
            } else {
                !bits // flip all bits for negative
            };
            Ok(sortable.to_be_bytes().to_vec())
        }
        _ => Err(EngineError::InvalidFieldType {
            field: format!("{field_type:?}"),
            expected: format!("{field_type:?}"),
            got: format!("{value:?}"),
        }),
    }
}

/// Encode multiple values as a compound key with null-byte separators.
pub fn encode_compound_key(
    values: &[Value],
    field_types: &[FieldType],
) -> Result<Vec<u8>, EngineError> {
    let mut key = Vec::new();
    for (value, ft) in values.iter().zip(field_types.iter()) {
        let encoded = encode_sortable(value, ft)?;
        key.extend_from_slice(&encoded);
    }
    Ok(key)
}

// ---------------------------------------------------------------------------
// FieldIndex — Fjall-backed sortable field index
// ---------------------------------------------------------------------------

pub struct FieldIndex {
    partition: fjall::PartitionHandle,
    field_types: Vec<(String, FieldType)>,
}

impl FieldIndex {
    pub fn new(
        partition: fjall::PartitionHandle,
        field_types: Vec<(String, FieldType)>,
    ) -> Self {
        Self { partition, field_types }
    }

    pub fn insert(&self, entity_id: &LogicalId, values: &[Value]) -> Result<(), EngineError> {
        let types: Vec<FieldType> = self.field_types.iter().map(|(_, t)| t.clone()).collect();
        let key = encode_compound_key(values, &types)?;
        // Append entity ID to key to allow multiple entities with same field values
        let mut full_key = key;
        full_key.push(0x00);
        full_key.extend_from_slice(entity_id.as_str().as_bytes());

        self.partition.insert(&full_key, entity_id.as_str().as_bytes())
            .map_err(|e| EngineError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn remove(&self, entity_id: &LogicalId, values: &[Value]) -> Result<(), EngineError> {
        let types: Vec<FieldType> = self.field_types.iter().map(|(_, t)| t.clone()).collect();
        let key = encode_compound_key(values, &types)?;
        let mut full_key = key;
        full_key.push(0x00);
        full_key.extend_from_slice(entity_id.as_str().as_bytes());

        self.partition.remove(&full_key)
            .map_err(|e| EngineError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn lookup_eq(&self, values: &[Value]) -> Result<Vec<LogicalId>, EngineError> {
        if values.len() != self.field_types.len() {
            return Err(EngineError::InvalidQuery(format!(
                "lookup_eq requires {} values, got {}",
                self.field_types.len(),
                values.len()
            )));
        }
        let types: Vec<FieldType> = self.field_types.iter().map(|(_, t)| t.clone()).collect();
        let prefix = encode_compound_key(values, &types)?;
        self.scan_prefix(&prefix)
    }

    pub fn lookup_prefix(&self, values: &[Value]) -> Result<Vec<LogicalId>, EngineError> {
        let types: Vec<FieldType> = self.field_types.iter()
            .take(values.len())
            .map(|(_, t)| t.clone())
            .collect();
        let prefix = encode_compound_key(values, &types)?;
        self.scan_prefix(&prefix)
    }

    pub fn lookup_range(
        &self,
        from: &[Value],
        to: &[Value],
    ) -> Result<Vec<LogicalId>, EngineError> {
        let types: Vec<FieldType> = self.field_types.iter().map(|(_, t)| t.clone()).collect();
        let from_key = encode_compound_key(from, &types)?;
        // Extend to_key with 0xFF bytes so the inclusive upper bound covers all
        // stored keys that have the compound prefix (e.g. {encoded_value}\x00{entity_id}).
        let mut to_key = encode_compound_key(to, &types)?;
        to_key.push(0xFF);
        to_key.extend_from_slice(&[0xFF; 64]); // generous padding

        let mut results = Vec::new();
        for kv in self.partition.range(from_key..=to_key) {
            let (_, v) = kv.map_err(|e| EngineError::Storage(e.to_string()))?;
            if let Ok(id_str) = std::str::from_utf8(&v) {
                results.push(LogicalId::from_string(id_str));
            }
        }
        Ok(results)
    }

    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<LogicalId>, EngineError> {
        let mut results = Vec::new();
        for kv in self.partition.prefix(prefix) {
            let (_, v) = kv.map_err(|e| EngineError::Storage(e.to_string()))?;
            if let Ok(id_str) = std::str::from_utf8(&v) {
                results.push(LogicalId::from_string(id_str));
            }
        }
        Ok(results)
    }

    pub fn field_types(&self) -> &[(String, FieldType)] {
        &self.field_types
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fjall::{Config, PartitionCreateOptions};
    use tempfile::TempDir;

    fn open_test_partition() -> (fjall::PartitionHandle, TempDir) {
        let dir = TempDir::new().unwrap();
        let ks = Config::new(dir.path()).open().unwrap();
        let part = ks.open_partition("test_idx", PartitionCreateOptions::default()).unwrap();
        (part, dir)
    }

    #[test]
    fn encode_text_sorts_lexicographically() {
        let a = encode_sortable(&Value::String("alpha".into()), &FieldType::Text).unwrap();
        let b = encode_sortable(&Value::String("beta".into()), &FieldType::Text).unwrap();
        assert!(a < b);
    }

    #[test]
    fn encode_int_negative_before_positive() {
        let neg = encode_sortable(&Value::Int(-10), &FieldType::Int).unwrap();
        let zero = encode_sortable(&Value::Int(0), &FieldType::Int).unwrap();
        let pos = encode_sortable(&Value::Int(10), &FieldType::Int).unwrap();
        assert!(neg < zero);
        assert!(zero < pos);
    }

    #[test]
    fn encode_int_preserves_order() {
        let a = encode_sortable(&Value::Int(100), &FieldType::Int).unwrap();
        let b = encode_sortable(&Value::Int(200), &FieldType::Int).unwrap();
        assert!(a < b);
    }

    #[test]
    fn encode_bool_false_before_true() {
        let f = encode_sortable(&Value::Bool(false), &FieldType::Bool).unwrap();
        let t = encode_sortable(&Value::Bool(true), &FieldType::Bool).unwrap();
        assert!(f < t);
    }

    #[test]
    fn encode_float_preserves_order() {
        let a = encode_sortable(&Value::Float(-1.5), &FieldType::Float).unwrap();
        let b = encode_sortable(&Value::Float(0.0), &FieldType::Float).unwrap();
        let c = encode_sortable(&Value::Float(1.5), &FieldType::Float).unwrap();
        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn encode_datetime_sorts_chronologically() {
        let a = encode_sortable(
            &Value::String("2024-01-01T00:00:00Z".into()),
            &FieldType::DateTime,
        ).unwrap();
        let b = encode_sortable(
            &Value::String("2024-06-15T12:00:00Z".into()),
            &FieldType::DateTime,
        ).unwrap();
        assert!(a < b);
    }

    #[test]
    fn encode_compound_key() {
        let key = super::encode_compound_key(
            &[Value::String("venue1".into()), Value::String("2024-01-01T00:00:00Z".into())],
            &[FieldType::Text, FieldType::DateTime],
        ).unwrap();
        assert!(key.contains(&0x00)); // null separator
    }

    #[test]
    fn compound_key_prefix_ordering() {
        let key_a = super::encode_compound_key(
            &[Value::String("a".into()), Value::String("2024-01-01".into())],
            &[FieldType::Text, FieldType::DateTime],
        ).unwrap();
        let key_b = super::encode_compound_key(
            &[Value::String("b".into()), Value::String("2024-01-01".into())],
            &[FieldType::Text, FieldType::DateTime],
        ).unwrap();
        assert!(key_a < key_b);
    }

    #[test]
    fn field_index_insert_and_lookup_eq() {
        let (partition, _dir) = open_test_partition();
        let idx = FieldIndex::new(
            partition,
            vec![("status".into(), FieldType::Text)],
        );
        let id1 = LogicalId::from_string("e1");
        let id2 = LogicalId::from_string("e2");

        idx.insert(&id1, &[Value::String("active".into())]).unwrap();
        idx.insert(&id2, &[Value::String("active".into())]).unwrap();

        let results = idx.lookup_eq(&[Value::String("active".into())]).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn field_index_lookup_eq_no_match() {
        let (partition, _dir) = open_test_partition();
        let idx = FieldIndex::new(
            partition,
            vec![("status".into(), FieldType::Text)],
        );
        let id1 = LogicalId::from_string("e1");
        idx.insert(&id1, &[Value::String("active".into())]).unwrap();

        let results = idx.lookup_eq(&[Value::String("inactive".into())]).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn field_index_remove() {
        let (partition, _dir) = open_test_partition();
        let idx = FieldIndex::new(
            partition,
            vec![("status".into(), FieldType::Text)],
        );
        let id1 = LogicalId::from_string("e1");
        idx.insert(&id1, &[Value::String("active".into())]).unwrap();
        idx.remove(&id1, &[Value::String("active".into())]).unwrap();

        let results = idx.lookup_eq(&[Value::String("active".into())]).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn field_index_compound_key_prefix_lookup() {
        let (partition, _dir) = open_test_partition();
        let idx = FieldIndex::new(
            partition,
            vec![
                ("venue_id".into(), FieldType::Text),
                ("start_date".into(), FieldType::DateTime),
            ],
        );
        let id1 = LogicalId::from_string("e1");
        let id2 = LogicalId::from_string("e2");
        let id3 = LogicalId::from_string("e3");

        idx.insert(&id1, &[Value::String("v1".into()), Value::String("2024-01-01".into())]).unwrap();
        idx.insert(&id2, &[Value::String("v1".into()), Value::String("2024-06-01".into())]).unwrap();
        idx.insert(&id3, &[Value::String("v2".into()), Value::String("2024-01-01".into())]).unwrap();

        // Prefix lookup on venue_id only
        let results = idx.lookup_prefix(&[Value::String("v1".into())]).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn field_index_range_lookup() {
        let (partition, _dir) = open_test_partition();
        let idx = FieldIndex::new(
            partition,
            vec![("score".into(), FieldType::Int)],
        );
        for i in 0..10 {
            let id = LogicalId::from_string(&format!("e{i}"));
            idx.insert(&id, &[Value::Int(i * 10)]).unwrap();
        }
        // Range 20..=50 should match e2, e3, e4, e5
        let results = idx.lookup_range(&[Value::Int(20)], &[Value::Int(50)]).unwrap();
        assert_eq!(results.len(), 4);
    }

    #[test]
    fn field_index_lookup_eq_wrong_count_fails() {
        let (partition, _dir) = open_test_partition();
        let idx = FieldIndex::new(
            partition,
            vec![
                ("a".into(), FieldType::Text),
                ("b".into(), FieldType::Text),
            ],
        );
        // lookup_eq with 1 value on a 2-field index should fail
        let result = idx.lookup_eq(&[Value::String("x".into())]);
        assert!(result.is_err());
    }
}
