use crate::types::LogicalId;
use std::collections::VecDeque;
use std::sync::Mutex;

/// A candidate edge proposed by the inference pipeline.
#[derive(Debug, Clone)]
pub struct InferenceCandidate {
    pub entity_id: LogicalId,
    pub similarity_score: f32,
    pub accepted: bool,
}

/// What triggered the inference.
#[derive(Debug, Clone, PartialEq)]
pub enum InferenceTrigger {
    Explicit,   // INFER EDGES query
    Background, // InferenceSweeper
}

/// One entry in the inference audit ring buffer.
#[derive(Debug, Clone)]
pub struct InferenceAuditEntry {
    pub timestamp: u64,
    pub source_entity: LogicalId,
    pub edge_type: String,
    pub candidates_evaluated: usize,
    pub candidates_above_threshold: usize,
    pub top_candidates: Vec<InferenceCandidate>,
    pub trigger: InferenceTrigger,
}

/// RAM-resident ring buffer of inference audit entries.
pub struct InferenceAuditBuffer {
    entries: Mutex<VecDeque<InferenceAuditEntry>>,
    capacity: usize,
}

impl InferenceAuditBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    pub fn record(&self, entry: InferenceAuditEntry) {
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= self.capacity {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    pub fn query(&self, entity_id: &LogicalId, limit: Option<usize>) -> Vec<InferenceAuditEntry> {
        let entries = self.entries.lock().unwrap();
        let limit = limit.unwrap_or(usize::MAX);
        entries
            .iter()
            .rev()
            .filter(|e| e.source_entity == *entity_id)
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(entity: &str, edge_type: &str) -> InferenceAuditEntry {
        InferenceAuditEntry {
            timestamp: 0,
            source_entity: LogicalId::from_string(entity),
            edge_type: edge_type.into(),
            candidates_evaluated: 5,
            candidates_above_threshold: 2,
            top_candidates: vec![],
            trigger: InferenceTrigger::Explicit,
        }
    }

    #[test]
    fn audit_buffer_records_and_queries() {
        let buf = InferenceAuditBuffer::new(100);
        buf.record(make_entry("e1", "likes"));
        buf.record(make_entry("e2", "likes"));
        buf.record(make_entry("e1", "visits"));

        let results = buf.query(&LogicalId::from_string("e1"), None);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn audit_buffer_evicts_oldest() {
        let buf = InferenceAuditBuffer::new(2);
        buf.record(make_entry("e1", "a"));
        buf.record(make_entry("e2", "b"));
        buf.record(make_entry("e3", "c"));

        assert_eq!(buf.len(), 2);
        let all = buf.query(&LogicalId::from_string("e1"), None);
        assert!(all.is_empty()); // e1 was evicted
    }

    #[test]
    fn audit_buffer_respects_limit() {
        let buf = InferenceAuditBuffer::new(100);
        for i in 0..20 {
            buf.record(make_entry("e1", &format!("type{i}")));
        }
        let results = buf.query(&LogicalId::from_string("e1"), Some(5));
        assert_eq!(results.len(), 5);
    }
}
