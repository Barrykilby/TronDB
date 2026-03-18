use std::sync::Arc;
use std::path::PathBuf;

use clap::Parser;
use tokio::signal;
use tokio::task::JoinHandle;
use tracing_subscriber::EnvFilter;
use trondb_proto::pb;

use trondb_server::config;
use trondb_server::service;

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
    tracing::info!("OpenTelemetry tracing available via OTEL_EXPORTER_OTLP_ENDPOINT env");

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

    // Register vectorisers for existing collections that have a VectoriserConfig
    register_vectorisers(&engine);

    let engine = Arc::new(engine);

    // Process pending WAL records (TierMigration + AffinityGroup)
    let affinity = trondb_routing::AffinityIndex::new();
    trondb_routing::startup::process_pending_wal_records(&engine, &pending_records, &affinity).await?;

    // Background task handles — add spawned tasks here as they are introduced.
    let mut background_handles: Vec<JoinHandle<()>> = Vec::new();

    // Spawn Prometheus metrics endpoint on port 9401
    background_handles.push(start_metrics_server(engine.clone(), 9401));

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
            if let Err(e) = engine_shutdown.flush_wal().await {
                tracing::error!(%e, "failed to flush WAL during shutdown");
            }

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
    let engine_config = config.to_engine_config();
    let (engine, _pending_records) = trondb_core::Engine::open(engine_config).await?;

    // Register vectorisers for existing collections that have a VectoriserConfig
    register_vectorisers(&engine);

    let engine = Arc::new(engine);

    // Find the primary peer in the cluster configuration
    let primary_peer = config
        .peers
        .iter()
        .find(|p| p.role == config::NodeRoleConfig::Primary)
        .ok_or("no primary peer configured")?;
    let primary_addr = format!("http://{}", primary_peer.addr);

    // Connect to the primary's gRPC endpoint for WAL streaming
    let mut client = pb::tron_node_client::TronNodeClient::connect(primary_addr.clone()).await?;

    // Start WAL catch-up + live streaming in a background task
    let wal_engine = engine.clone();
    let wal_handle = tokio::spawn(async move {
        let last_lsn = wal_engine.last_applied_lsn();
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        // Send initial WalAck with our last applied LSN
        if let Err(e) = tx.send(pb::WalAck { confirmed_lsn: last_lsn }).await {
            tracing::error!("failed to send initial WalAck: {e}");
            return;
        }

        let response = match client
            .stream_wal(tokio_stream::wrappers::ReceiverStream::new(rx))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("failed to start WAL stream: {e}");
                return;
            }
        };
        let mut stream = response.into_inner();

        loop {
            let wal_msg = match stream.message().await {
                Ok(Some(msg)) => msg,
                Ok(None) => break,
                Err(e) => {
                    tracing::error!("WAL stream error: {e}");
                    break;
                }
            };
            let record = match trondb_wal::WalRecord::try_from(wal_msg) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("invalid WAL record from primary: {e}");
                    continue;
                }
            };
            let lsn = record.lsn;
            if let Err(e) = wal_engine.apply_wal_record(&record).await {
                tracing::error!(lsn, "failed to apply WAL record: {e}");
            }
            // Acknowledge this LSN to the primary (best-effort)
            let _ = tx.send(pb::WalAck { confirmed_lsn: lsn }).await;
        }
        tracing::warn!("WAL stream from primary ended");
    });

    // Build gRPC service for the replica (serves read queries, forwards writes)
    let primary_channel = tonic::transport::Channel::from_shared(primary_addr)?
        .connect()
        .await?;
    let service = service::TronNodeService::new(engine.clone(), config::NodeRoleConfig::Replica)
        .with_primary(primary_channel);

    let addr = config.bind_addr.parse()?;

    // gRPC health probe
    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<pb::tron_node_server::TronNodeServer<service::TronNodeService>>()
        .await;

    tracing::info!(%addr, "replica listening");

    let engine_shutdown = engine.clone();
    tonic::transport::Server::builder()
        .add_service(health_service)
        .add_service(pb::tron_node_server::TronNodeServer::new(service))
        .serve_with_shutdown(addr, async move {
            shutdown_signal().await;
            tracing::info!("replica graceful shutdown starting");

            // Abort WAL streaming task
            wal_handle.abort();

            // Flush WAL
            if let Err(e) = engine_shutdown.flush_wal().await {
                tracing::error!(%e, "failed to flush WAL during shutdown");
            }

            // Save HNSW snapshots
            if let Err(e) = engine_shutdown.save_hnsw_snapshots() {
                tracing::error!(%e, "failed to save HNSW snapshots during shutdown");
            }

            tracing::info!("replica shutdown complete");
        })
        .await?;

    Ok(())
}

async fn start_router(config: config::ClusterConfig) -> Result<(), Box<dyn std::error::Error>> {
    // Find the primary peer in the cluster configuration
    let primary_peer = config
        .peers
        .iter()
        .find(|p| p.role == config::NodeRoleConfig::Primary)
        .ok_or("no primary peer configured")?;
    let primary_addr = format!("http://{}", primary_peer.addr);

    // Connect to the primary's gRPC endpoint
    let primary_channel = tonic::transport::Channel::from_shared(primary_addr)?
        .connect()
        .await?;

    // Router has no local Engine — uses new_router constructor that forwards
    // all Execute calls to the primary
    let service = service::TronNodeService::new_router(primary_channel);

    let addr = config.bind_addr.parse()?;

    // gRPC health probe
    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<pb::tron_node_server::TronNodeServer<service::TronNodeService>>()
        .await;

    tracing::info!(%addr, "router listening");

    tonic::transport::Server::builder()
        .add_service(health_service)
        .add_service(pb::tron_node_server::TronNodeServer::new(service))
        .serve_with_shutdown(addr, async move {
            shutdown_signal().await;
            tracing::info!("router shutdown complete");
        })
        .await?;

    Ok(())
}

/// Spawn a lightweight TCP server that serves Prometheus metrics on the given port.
///
/// Responds to any TCP connection with an HTTP/1.1 200 response containing the
/// Prometheus text exposition format from `EngineMetrics::render()`.
fn start_metrics_server(engine: Arc<trondb_core::Engine>, port: u16) -> JoinHandle<()> {
    tokio::spawn(async move {
        let addr = format!("127.0.0.1:{port}");
        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(port, %e, "failed to bind metrics server");
                return;
            }
        };
        tracing::info!(port, "metrics server listening");
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                let metrics_body = engine.metrics().render();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    metrics_body.len(),
                    metrics_body,
                );
                // Best-effort write — ignore errors from disconnected clients
                let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
            }
        }
    })
}

/// Register vectorisers for all existing collections that have a `VectoriserConfig`,
/// and install a collection lifecycle hook so that runtime CreateCollection /
/// DropCollection also register/deregister vectorisers automatically.
///
/// Called during startup (before `Arc::new(engine)`) so that the engine can
/// auto-vectorise inserts and handle SEARCH NEAR 'text' queries immediately.
fn register_vectorisers(engine: &trondb_core::Engine) {
    // 1. Register vectorisers for collections that already exist on disk.
    for schema in engine.schemas() {
        register_vectorisers_for_schema(engine.vectoriser_registry(), &schema);
    }

    // 2. Install a hook so that CreateCollection / DropCollection at runtime
    //    also register/deregister vectorisers — regardless of calling path
    //    (gRPC, execute_tql, CLI, replica WAL replay).
    let registry = std::sync::Arc::clone(engine.vectoriser_registry());
    engine.set_collection_hook(Box::new(move |event| {
        use trondb_core::executor::CollectionEvent;
        match event {
            CollectionEvent::Created(schema) => {
                register_vectorisers_for_schema(&registry, schema);
            }
            CollectionEvent::Dropped(name) => {
                registry.remove_collection(name);
            }
        }
    }));
}

/// Register vectorisers for every managed representation in a single schema.
fn register_vectorisers_for_schema(
    registry: &trondb_core::vectoriser::VectoriserRegistry,
    schema: &trondb_core::types::CollectionSchema,
) {
    if let Some(ref vc) = schema.vectoriser_config {
        for repr in &schema.representations {
            if !repr.fields.is_empty() {
                match trondb_vectoriser::create_vectoriser_from_config(vc, repr) {
                    Ok(v) => registry.register(&schema.name, &repr.name, v),
                    Err(e) => tracing::warn!(
                        collection = %schema.name,
                        repr = %repr.name,
                        "could not create vectoriser: {e}",
                    ),
                }
            }
        }
    }
}
