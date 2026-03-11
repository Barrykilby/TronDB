use std::sync::Arc;
use trondb_core::Engine;
use trondb_core::edge::effective_confidence;

pub struct DecaySweeper {
    pub(crate) engine: Arc<Engine>,
    pub(crate) interval_secs: u64,
}

impl DecaySweeper {
    pub fn new(engine: Arc<Engine>, interval_secs: u64) -> Self {
        Self { engine, interval_secs }
    }

    /// Run one sweep cycle: scan edge types with decay config, prune expired edges.
    pub async fn sweep_once(&self) -> usize {
        let edge_types = self.engine.list_edge_types();
        let mut pruned = 0;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        for et in &edge_types {
            if et.decay_config.prune_threshold.is_none() {
                continue;
            }
            let prune_threshold = et.decay_config.prune_threshold.unwrap();

            let edges = match self.engine.scan_edges(&et.name) {
                Ok(e) => e,
                Err(_) => continue,
            };

            for edge in &edges {
                if edge.created_at == 0 {
                    continue; // legacy edge, no decay
                }
                let elapsed = now_ms.saturating_sub(edge.created_at);
                let conf = effective_confidence(edge.confidence, elapsed, &et.decay_config);
                if conf < prune_threshold as f32 {
                    // Delete via TQL execution (single quotes!)
                    let tql = format!(
                        "DELETE EDGE {} FROM '{}' TO '{}';",
                        et.name,
                        edge.from_id.as_str(),
                        edge.to_id.as_str()
                    );
                    if let Ok(plan) = self.engine.parse_and_plan(&tql) {
                        let _ = self.engine.execute(&plan).await;
                        pruned += 1;
                    }
                }
            }
        }

        pruned
    }

    /// Spawn the background sweep loop.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(self.interval_secs)).await;
                let pruned = self.sweep_once().await;
                if pruned > 0 {
                    eprintln!("[DecaySweeper] pruned {pruned} expired edges");
                }
            }
        })
    }
}
