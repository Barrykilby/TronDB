//! Slow query log — queries exceeding a configurable ACU threshold are recorded.
//!
//! The log is a bounded ring buffer that can be queried programmatically or
//! exported as structured log lines.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use crate::cost::AcuEstimate;

/// A single slow query log entry.
#[derive(Debug, Clone)]
pub struct SlowQueryEntry {
    pub query_text: String,
    pub estimated_acu: f64,
    pub elapsed: Duration,
    pub timestamp_ms: i64,
    pub cost_breakdown: String,
}

/// Configuration for the slow query log.
#[derive(Debug, Clone, Copy)]
pub struct SlowQueryConfig {
    /// Queries with estimated ACU above this threshold are logged.
    pub acu_threshold: f64,
    /// Maximum number of entries to retain.
    pub max_entries: usize,
}

impl Default for SlowQueryConfig {
    fn default() -> Self {
        Self {
            acu_threshold: 100.0, // default: 100 ACU
            max_entries: 1000,
        }
    }
}

/// Ring buffer for slow query entries.
pub struct SlowQueryLog {
    config: SlowQueryConfig,
    entries: Mutex<VecDeque<SlowQueryEntry>>,
}

impl SlowQueryLog {
    pub fn new(config: SlowQueryConfig) -> Self {
        Self {
            config,
            entries: Mutex::new(VecDeque::with_capacity(config.max_entries)),
        }
    }

    /// Check if a query should be logged, and if so, record it.
    pub fn maybe_log(
        &self,
        query_text: &str,
        estimate: &AcuEstimate,
        elapsed: Duration,
    ) {
        if estimate.total_acu < self.config.acu_threshold {
            return;
        }

        let breakdown = estimate
            .items
            .iter()
            .map(|item| {
                format!(
                    "{}x {} @ {:.1} = {:.1}",
                    item.count, item.operation, item.unit_cost, item.total
                )
            })
            .collect::<Vec<_>>()
            .join("; ");

        let entry = SlowQueryEntry {
            query_text: query_text.to_string(),
            estimated_acu: estimate.total_acu,
            elapsed,
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64,
            cost_breakdown: breakdown,
        };

        tracing::warn!(
            acu = entry.estimated_acu,
            elapsed_ms = entry.elapsed.as_millis() as u64,
            query = %entry.query_text,
            breakdown = %entry.cost_breakdown,
            "slow query detected"
        );

        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= self.config.max_entries {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// Return all entries currently in the log.
    ///
    /// **Note:** This clones the entire ring buffer under the mutex.
    /// Suitable for infrequent admin/diagnostic calls; avoid on hot paths.
    pub fn entries(&self) -> Vec<SlowQueryEntry> {
        self.entries.lock().unwrap().iter().cloned().collect()
    }

    /// Return the number of entries in the log.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    /// Returns true if the log is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.lock().unwrap().is_empty()
    }

    /// Get the ACU threshold.
    pub fn threshold(&self) -> f64 {
        self.config.acu_threshold
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::AcuEstimate;

    #[test]
    fn below_threshold_not_logged() {
        let log = SlowQueryLog::new(SlowQueryConfig {
            acu_threshold: 100.0,
            max_entries: 10,
        });
        let est = AcuEstimate::single("fetch_hot", 10, 1.0); // 10 ACU
        log.maybe_log("FETCH * FROM venues;", &est, Duration::from_millis(5));
        assert!(log.is_empty());
    }

    #[test]
    fn above_threshold_logged() {
        let log = SlowQueryLog::new(SlowQueryConfig {
            acu_threshold: 50.0,
            max_entries: 10,
        });
        let est = AcuEstimate::single("full_scan", 200, 0.5); // 100 ACU
        log.maybe_log("FETCH * FROM venues;", &est, Duration::from_millis(50));
        assert_eq!(log.len(), 1);
        let entries = log.entries();
        assert!((entries[0].estimated_acu - 100.0).abs() < f64::EPSILON);
        assert!(entries[0].query_text.contains("FETCH"));
    }

    #[test]
    fn ring_buffer_evicts_oldest() {
        let log = SlowQueryLog::new(SlowQueryConfig {
            acu_threshold: 1.0,
            max_entries: 3,
        });
        for i in 0..5 {
            let est = AcuEstimate::single("op", 10, 1.0);
            log.maybe_log(&format!("query_{i}"), &est, Duration::from_millis(1));
        }
        assert_eq!(log.len(), 3);
        let entries = log.entries();
        // Oldest entries (0, 1) should have been evicted
        assert!(entries[0].query_text.contains("query_2"));
        assert!(entries[2].query_text.contains("query_4"));
    }

    #[test]
    fn exactly_at_threshold_is_logged() {
        let log = SlowQueryLog::new(SlowQueryConfig {
            acu_threshold: 100.0,
            max_entries: 10,
        });
        // 100.0 < 100.0 is false, so exactly at threshold should be logged
        let est = AcuEstimate::single("full_scan", 200, 0.5); // 100 ACU
        log.maybe_log("query", &est, Duration::from_millis(1));
        assert_eq!(log.len(), 1);
    }
}
