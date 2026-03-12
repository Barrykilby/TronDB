use std::sync::Arc;
use std::path::PathBuf;

use clap::Parser;
use tokio::signal;
use tokio::task::JoinHandle;
use tracing_subscriber::EnvFilter;
use trondb_proto::pb;

pub mod config;
pub mod location_stream;
pub mod metrics;
pub mod remote_node;
pub mod replication;
pub mod scatter;
pub mod service;
pub mod stream_health;
pub mod write_forward;

#[derive(Parser)]
#[command(name = "trondb-server", about = "TronDB distributed node")]
struct Cli {
    /// Path to config TOML file
    #[arg(long, env = "TRONDB_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Structured JSON logging
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    // Load config: file → env overrides
    let mut config = match cli.config {
        Some(path) => config::ClusterConfig::from_file(&path)?,
        None => config::ClusterConfig::from_env()?,
    };
    config.apply_env_overrides();

    tracing::info!(node_id = %config.node_id, role = ?config.role, "starting trondb-server");

    match config.role {
        config::NodeRoleConfig::Primary => start_primary(config).await?,
        config::NodeRoleConfig::Replica => start_replica(config).await?,
        config::NodeRoleConfig::Router => start_router(config).await?,
    }

    Ok(())
}

/// Wait for SIGINT (Ctrl+C) or SIGTERM, then return.
async fn shutdown_signal() {
    let ctrl_c = signal::ctrl_c();
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate()).unwrap();
    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT"),
        _ = sigterm.recv() => tracing::info!("received SIGTERM"),
    }
}

async fn start_primary(config: config::ClusterConfig) -> Result<(), Box<dyn std::error::Error>> {
    let engine_config = config.to_engine_config();
    let (engine, pending_records) = trondb_core::Engine::open(engine_config).await?;
    let engine = Arc::new(engine);

    // Process pending WAL records (TierMigration + AffinityGroup)
    let affinity = trondb_routing::AffinityIndex::new();
    trondb_routing::startup::process_pending_wal_records(&engine, &pending_records, &affinity).await?;

    // Background task handles — add spawned tasks here as they are introduced.
    let background_handles: Vec<JoinHandle<()>> = Vec::new();

    let service = service::TronNodeService::new(engine.clone(), config.role);
    let addr = config.bind_addr.parse()?;

    // gRPC health probe (tonic-health)
    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<pb::tron_node_server::TronNodeServer<service::TronNodeService>>()
        .await;

    tracing::info!(%addr, "primary listening");

    let engine_shutdown = engine.clone();
    tonic::transport::Server::builder()
        .add_service(health_service)
        .add_service(pb::tron_node_server::TronNodeServer::new(service))
        .serve_with_shutdown(addr, async move {
            shutdown_signal().await;
            tracing::info!("graceful shutdown starting");

            // 1. Abort background tasks
            for h in &background_handles {
                h.abort();
            }

            // 2. Flush WAL
            // engine_shutdown.flush_wal() — added in Task 20

            // 3. Save HNSW snapshots
            if let Err(e) = engine_shutdown.save_hnsw_snapshots() {
                tracing::error!(%e, "failed to save HNSW snapshots during shutdown");
            }

            tracing::info!("shutdown complete");
        })
        .await?;
    Ok(())
}

async fn start_replica(config: config::ClusterConfig) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("start_replica stub — completed in Task 20");
    let _ = config;
    Ok(())
}

async fn start_router(config: config::ClusterConfig) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("start_router stub — completed in Task 20");
    let _ = config;
    Ok(())
}
