use std::sync::Arc;
use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;
use trondb_proto::pb;

pub mod config;
pub mod remote_node;
pub mod replication;
pub mod service;
pub mod stream_health;

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

async fn start_primary(config: config::ClusterConfig) -> Result<(), Box<dyn std::error::Error>> {
    let engine_config = config.to_engine_config();
    let (engine, _pending_records) = trondb_core::Engine::open(engine_config).await?;
    let engine = Arc::new(engine);

    // Process pending WAL records (TierMigration + AffinityGroup)
    // (wired in Task 18)

    let service = service::TronNodeService::new(engine.clone(), config.role);
    let addr = config.bind_addr.parse()?;

    tracing::info!(%addr, "primary listening");
    tonic::transport::Server::builder()
        .add_service(pb::tron_node_server::TronNodeServer::new(service))
        .serve(addr)
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
