//! Real CPU/RAM metrics via `sysinfo`, rolling percentile tracker, and
//! atomic query counter with RAII guard.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use sysinfo::System;

// ---------------------------------------------------------------------------
// SystemMetrics — real CPU + RAM readings
// ---------------------------------------------------------------------------

/// Wraps [`sysinfo::System`] behind a [`Mutex`] to provide thread-safe,
/// on-demand CPU utilisation and RAM pressure readings.
pub struct SystemMetrics {
    system: Mutex<System>,
}

impl Default for SystemMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemMetrics {
    /// Create a new `SystemMetrics` instance. An initial CPU refresh is
    /// performed so that the first call to [`cpu_utilisation`] returns a
    /// meaningful delta rather than zero.
    pub fn new() -> Self {
        let mut sys = System::new();
        // Prime the CPU usage calculation — sysinfo needs two data points.
        sys.refresh_cpu_usage();
        Self {
            system: Mutex::new(sys),
        }
    }

    /// Returns global CPU utilisation in the range `0.0..=1.0`.
    pub fn cpu_utilisation(&self) -> f32 {
        let mut sys = self.system.lock().unwrap();
        sys.refresh_cpu_usage();
        (sys.global_cpu_usage() / 100.0).clamp(0.0, 1.0)
    }

    /// Returns RAM pressure (used / total) in the range `0.0..=1.0`.
    pub fn ram_pressure(&self) -> f32 {
        let mut sys = self.system.lock().unwrap();
        sys.refresh_memory();
        let total = sys.total_memory();
        if total == 0 {
            return 0.0;
        }
        (sys.used_memory() as f64 / total as f64) as f32
    }
}

// ---------------------------------------------------------------------------
// RollingPercentile — ring-buffer of recent latencies
// ---------------------------------------------------------------------------

/// A fixed-capacity ring buffer that records latency samples and computes
/// approximate percentiles (e.g. p50, p99) by sorting the current window.
pub struct RollingPercentile {
    buffer: Vec<f32>,
    capacity: usize,
    cursor: usize,
    full: bool,
}

impl RollingPercentile {
    /// Create a new ring buffer with the given maximum capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(capacity),
            capacity,
            cursor: 0,
            full: false,
        }
    }

    /// Record a latency value into the ring buffer, overwriting the oldest
    /// entry once the buffer is full.
    pub fn record(&mut self, value: f32) {
        if self.buffer.len() < self.capacity {
            self.buffer.push(value);
        } else {
            self.buffer[self.cursor] = value;
            self.full = true;
        }
        self.cursor = (self.cursor + 1) % self.capacity;
    }

    /// Compute the `p`-th percentile (e.g. `p=50` for p50, `p=99` for p99).
    ///
    /// Returns `0.0` when the buffer is empty.
    pub fn percentile(&self, p: usize) -> f32 {
        let len = self.buffer.len();
        if len == 0 {
            return 0.0;
        }

        let mut sorted = self.buffer.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let idx = (p as f64 / 100.0 * len as f64) as usize;
        let idx = idx.min(len - 1);
        sorted[idx]
    }
}

// ---------------------------------------------------------------------------
// QueryCounter — atomic counter with RAII guard
// ---------------------------------------------------------------------------

/// An atomic counter tracking the number of in-flight queries. The count is
/// incremented via [`QueryCounter::enter`] and automatically decremented
/// when the returned [`QueryCounterGuard`] is dropped.
pub struct QueryCounter {
    count: AtomicU32,
}

impl Default for QueryCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryCounter {
    /// Create a new counter initialised to zero.
    pub fn new() -> Self {
        Self {
            count: AtomicU32::new(0),
        }
    }

    /// Increment the counter and return a guard that will decrement it on drop.
    pub fn enter(&self) -> QueryCounterGuard<'_> {
        self.count.fetch_add(1, Ordering::Relaxed);
        QueryCounterGuard { counter: self }
    }

    /// Return the current number of in-flight queries.
    pub fn current(&self) -> u32 {
        self.count.load(Ordering::Relaxed)
    }
}

/// RAII guard that decrements the parent [`QueryCounter`] when dropped.
pub struct QueryCounterGuard<'a> {
    counter: &'a QueryCounter,
}

impl<'a> Drop for QueryCounterGuard<'a> {
    fn drop(&mut self) {
        self.counter.count.fetch_sub(1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_metrics_returns_valid_ranges() {
        let metrics = SystemMetrics::new();
        let cpu = metrics.cpu_utilisation();
        let ram = metrics.ram_pressure();
        assert!(cpu >= 0.0 && cpu <= 1.0, "cpu={cpu}");
        assert!(ram >= 0.0 && ram <= 1.0, "ram={ram}");
    }

    #[test]
    fn rolling_percentile_tracks_values() {
        let mut rp = RollingPercentile::new(100);
        for i in 0..100 {
            rp.record(i as f32);
        }
        let p99 = rp.percentile(99);
        assert!(p99 >= 98.0, "p99={p99}");
        let p50 = rp.percentile(50);
        assert!(p50 >= 49.0 && p50 <= 51.0, "p50={p50}");
    }

    #[test]
    fn rolling_percentile_empty_returns_zero() {
        let rp = RollingPercentile::new(10);
        assert_eq!(rp.percentile(50), 0.0);
        assert_eq!(rp.percentile(99), 0.0);
    }

    #[test]
    fn rolling_percentile_wraps_around() {
        let mut rp = RollingPercentile::new(5);
        // Fill buffer
        for i in 0..5 {
            rp.record(i as f32);
        }
        // Overwrite oldest entries
        rp.record(100.0);
        rp.record(200.0);
        // Buffer should be [100, 200, 2, 3, 4] (ring wrapped)
        let p99 = rp.percentile(99);
        assert!(p99 >= 100.0, "p99={p99} after wrap");
    }

    #[test]
    fn query_counter_increments_and_decrements() {
        let counter = QueryCounter::new();
        assert_eq!(counter.current(), 0);

        let guard1 = counter.enter();
        assert_eq!(counter.current(), 1);

        let guard2 = counter.enter();
        assert_eq!(counter.current(), 2);

        drop(guard1);
        assert_eq!(counter.current(), 1);

        drop(guard2);
        assert_eq!(counter.current(), 0);
    }
}
