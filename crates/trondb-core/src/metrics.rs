//! Prometheus-compatible metrics registry.
//!
//! Lightweight, lock-free counters and gauges using atomics.
//! Exposes a `render()` method that produces Prometheus text format.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Counter — monotonically increasing
// ---------------------------------------------------------------------------

pub struct Counter {
    name: &'static str,
    help: &'static str,
    value: AtomicU64,
}

impl Counter {
    pub const fn new(name: &'static str, help: &'static str) -> Self {
        Self {
            name,
            help,
            value: AtomicU64::new(0),
        }
    }

    pub fn inc(&self) {
        self.value.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_by(&self, n: u64) {
        self.value.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn render(&self) -> String {
        format!(
            "# HELP {} {}\n# TYPE {} counter\n{} {}\n",
            self.name, self.help, self.name, self.name, self.get()
        )
    }
}

// ---------------------------------------------------------------------------
// Gauge — can go up and down
// ---------------------------------------------------------------------------

pub struct Gauge {
    name: &'static str,
    help: &'static str,
    value: AtomicI64,
}

impl Gauge {
    pub const fn new(name: &'static str, help: &'static str) -> Self {
        Self {
            name,
            help,
            value: AtomicI64::new(0),
        }
    }

    pub fn set(&self, v: i64) {
        self.value.store(v, Ordering::Relaxed);
    }

    pub fn inc(&self) {
        self.value.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec(&self) {
        self.value.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn get(&self) -> i64 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn render(&self) -> String {
        format!(
            "# HELP {} {}\n# TYPE {} gauge\n{} {}\n",
            self.name, self.help, self.name, self.name, self.get()
        )
    }
}

// ---------------------------------------------------------------------------
// Histogram — bucket-based latency distribution
// ---------------------------------------------------------------------------

pub struct Histogram {
    name: &'static str,
    help: &'static str,
    /// Sorted upper bounds for buckets (in seconds).
    bounds: &'static [f64],
    /// Counts per bucket + one for +Inf.
    buckets: Vec<AtomicU64>,
    sum: Mutex<f64>,
    count: AtomicU64,
}

/// Default latency buckets (in seconds).
pub const DEFAULT_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

impl Histogram {
    pub fn new(name: &'static str, help: &'static str, bounds: &'static [f64]) -> Self {
        let mut buckets = Vec::with_capacity(bounds.len() + 1);
        for _ in 0..=bounds.len() {
            buckets.push(AtomicU64::new(0));
        }
        Self {
            name,
            help,
            bounds,
            buckets,
            sum: Mutex::new(0.0),
            count: AtomicU64::new(0),
        }
    }

    pub fn observe(&self, value_secs: f64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        {
            let mut s = self.sum.lock().unwrap();
            *s += value_secs;
        }
        // Find the first bucket whose bound >= value and increment only that one.
        // The render() method computes cumulative sums across buckets.
        let mut placed = false;
        for (i, &bound) in self.bounds.iter().enumerate() {
            if value_secs <= bound {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
                placed = true;
                break;
            }
        }
        if !placed {
            // Value exceeds all finite bounds — goes only in the +Inf bucket
            self.buckets[self.bounds.len()].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn observe_duration(&self, duration: Duration) {
        self.observe(duration.as_secs_f64());
    }

    pub fn render(&self) -> String {
        let mut out = format!(
            "# HELP {} {}\n# TYPE {} histogram\n",
            self.name, self.help, self.name
        );
        let mut cumulative = 0u64;
        for (i, &bound) in self.bounds.iter().enumerate() {
            cumulative += self.buckets[i].load(Ordering::Relaxed);
            out.push_str(&format!(
                "{}_bucket{{le=\"{}\"}} {}\n",
                self.name, bound, cumulative
            ));
        }
        cumulative += self.buckets[self.bounds.len()].load(Ordering::Relaxed);
        out.push_str(&format!(
            "{}_bucket{{le=\"+Inf\"}} {}\n",
            self.name, cumulative
        ));
        let sum = *self.sum.lock().unwrap();
        out.push_str(&format!("{}_sum {}\n", self.name, sum));
        out.push_str(&format!(
            "{}_count {}\n",
            self.name,
            self.count.load(Ordering::Relaxed)
        ));
        out
    }
}

// ---------------------------------------------------------------------------
// EngineMetrics — all metrics for the engine
// ---------------------------------------------------------------------------

pub struct EngineMetrics {
    pub queries_total: Counter,
    pub queries_error_total: Counter,
    pub inserts_total: Counter,
    pub entities_total: Gauge,
    pub collections_total: Gauge,
    pub wal_writes_total: Counter,
    pub query_duration_seconds: Histogram,
    pub inflight_queries: Gauge,
}

impl EngineMetrics {
    pub fn new() -> Self {
        Self {
            queries_total: Counter::new(
                "trondb_queries_total",
                "Total number of queries executed",
            ),
            queries_error_total: Counter::new(
                "trondb_queries_error_total",
                "Total number of queries that returned an error",
            ),
            inserts_total: Counter::new(
                "trondb_inserts_total",
                "Total number of INSERT/UPSERT operations",
            ),
            entities_total: Gauge::new(
                "trondb_entities_total",
                "Current total number of entities across all collections",
            ),
            collections_total: Gauge::new(
                "trondb_collections_total",
                "Current number of collections",
            ),
            wal_writes_total: Counter::new(
                "trondb_wal_writes_total",
                "Total number of WAL records written",
            ),
            query_duration_seconds: Histogram::new(
                "trondb_query_duration_seconds",
                "Query execution latency in seconds",
                DEFAULT_BUCKETS,
            ),
            inflight_queries: Gauge::new(
                "trondb_inflight_queries",
                "Number of queries currently executing",
            ),
        }
    }

    /// Render all metrics in Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.queries_total.render());
        out.push_str(&self.queries_error_total.render());
        out.push_str(&self.inserts_total.render());
        out.push_str(&self.entities_total.render());
        out.push_str(&self.collections_total.render());
        out.push_str(&self.wal_writes_total.render());
        out.push_str(&self.query_duration_seconds.render());
        out.push_str(&self.inflight_queries.render());
        out
    }
}

impl Default for EngineMetrics {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_inc_and_get() {
        let c = Counter::new("test_counter", "A test counter");
        assert_eq!(c.get(), 0);
        c.inc();
        assert_eq!(c.get(), 1);
        c.inc_by(5);
        assert_eq!(c.get(), 6);
    }

    #[test]
    fn counter_render_format() {
        let c = Counter::new("test_counter", "A test counter");
        c.inc_by(42);
        let rendered = c.render();
        assert!(rendered.contains("# HELP test_counter A test counter"));
        assert!(rendered.contains("# TYPE test_counter counter"));
        assert!(rendered.contains("test_counter 42"));
    }

    #[test]
    fn gauge_set_inc_dec() {
        let g = Gauge::new("test_gauge", "A test gauge");
        assert_eq!(g.get(), 0);
        g.set(10);
        assert_eq!(g.get(), 10);
        g.inc();
        assert_eq!(g.get(), 11);
        g.dec();
        assert_eq!(g.get(), 10);
    }

    #[test]
    fn gauge_render_format() {
        let g = Gauge::new("test_gauge", "A test gauge");
        g.set(7);
        let rendered = g.render();
        assert!(rendered.contains("# TYPE test_gauge gauge"));
        assert!(rendered.contains("test_gauge 7"));
    }

    #[test]
    fn histogram_observe_and_render() {
        let h = Histogram::new("test_hist", "A test histogram", DEFAULT_BUCKETS);
        h.observe(0.002); // falls in 0.005 bucket
        h.observe(0.05);  // falls in 0.05 bucket
        h.observe(1.5);   // falls in 2.5 bucket

        let rendered = h.render();
        assert!(rendered.contains("# TYPE test_hist histogram"));
        assert!(rendered.contains("test_hist_count 3"));
        assert!(rendered.contains("test_hist_bucket{le=\"+Inf\"} 3"));
    }

    #[test]
    fn histogram_observe_duration() {
        let h = Histogram::new("test_dur", "A test", DEFAULT_BUCKETS);
        h.observe_duration(Duration::from_millis(50)); // 0.05s
        assert_eq!(h.count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn engine_metrics_render_all() {
        let m = EngineMetrics::new();
        m.queries_total.inc();
        m.inserts_total.inc_by(5);
        m.entities_total.set(100);
        m.collections_total.set(3);

        let rendered = m.render();
        assert!(rendered.contains("trondb_queries_total 1"));
        assert!(rendered.contains("trondb_inserts_total 5"));
        assert!(rendered.contains("trondb_entities_total 100"));
        assert!(rendered.contains("trondb_collections_total 3"));
    }
}
