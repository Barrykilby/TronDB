#[derive(Debug, Clone)]
pub struct RouterConfig {
    pub health: HealthConfig,
    pub colocation: ColocationConfig,
    pub weight_health: f32,
    pub weight_verb_fit: f32,
    pub weight_affinity: f32,
    pub affinity_boost_max: f32,
    pub retry_after_ms: u64,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            health: HealthConfig::default(),
            colocation: ColocationConfig::default(),
            weight_health: 0.40,
            weight_verb_fit: 0.30,
            weight_affinity: 0.30,
            affinity_boost_max: 0.05,
            retry_after_ms: 500,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HealthConfig {
    pub push_interval_ms: u64,
    pub stale_multiplier: u32,
    pub pull_threshold_acu: f32,
    pub cpu_warn_threshold: f32,
    pub ram_warn_threshold: f32,
    pub queue_depth_alert: u32,
    pub hnsw_baseline_p99_ms: f32,
    pub max_replica_lag_ms: f32,
    pub load_shed_threshold: f32,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            push_interval_ms: 200,
            stale_multiplier: 3,
            pull_threshold_acu: 80.0,
            cpu_warn_threshold: 0.75,
            ram_warn_threshold: 0.85,
            queue_depth_alert: 500,
            hnsw_baseline_p99_ms: 10.0,
            max_replica_lag_ms: 1000.0,
            load_shed_threshold: 0.85,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ColocationConfig {
    pub learn_threshold: f32,
    pub learn_interval_ms: u64,
    pub decay_factor: f32,
    pub max_group_size: usize,
    pub ram_headroom: f32,
    pub soft_evict_delay_ms: u64,
}

impl Default for ColocationConfig {
    fn default() -> Self {
        Self {
            learn_threshold: 0.70,
            learn_interval_ms: 30_000,
            decay_factor: 0.95,
            max_group_size: 500,
            ram_headroom: 0.80,
            soft_evict_delay_ms: 5_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TierConfig {
    /// Max entities in hot tier per collection (default: 100_000)
    pub hot_capacity: usize,
    /// Max entities in warm tier per collection (default: 1_000_000)
    pub warm_capacity: usize,
    /// Seconds idle before hot → warm demotion (default: 86400 = 24h)
    pub demote_after_secs: u64,
    /// Seconds idle in warm before warm → archive (default: 604800 = 7d)
    pub archive_after_secs: u64,
    /// Auto-promote warm → hot on FETCH (default: true)
    pub promote_on_access: bool,
    /// Max entities to migrate per cycle (default: 100)
    pub migration_batch_size: usize,
    /// Migration cycle interval in ms (default: 60_000)
    pub migration_interval_ms: u64,
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            hot_capacity: 100_000,
            warm_capacity: 1_000_000,
            demote_after_secs: 86_400,
            archive_after_secs: 604_800,
            promote_on_access: true,
            migration_batch_size: 100,
            migration_interval_ms: 60_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_weights_sum_to_one() {
        let cfg = RouterConfig::default();
        let sum = cfg.weight_health + cfg.weight_verb_fit + cfg.weight_affinity;
        assert!((sum - 1.0).abs() < 1e-6);
    }

    #[test]
    fn default_health_config_thresholds() {
        let cfg = HealthConfig::default();
        assert_eq!(cfg.push_interval_ms, 200);
        assert!((cfg.load_shed_threshold - 0.85).abs() < 1e-6);
    }
}
