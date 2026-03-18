mod display;

use std::path::PathBuf;
use std::sync::Arc;

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use trondb_core::{Engine, EngineConfig};
use trondb_routing::config::TierConfig;
use trondb_routing::migrator::TierMigrator;
use trondb_routing::node::{LocalNode, NodeId};
use trondb_routing::router::SemanticRouter;
use trondb_routing::sweeper::DecaySweeper;
use trondb_routing::RouterConfig;
use trondb_wal::WalConfig;

/// Parse a `--flag value` pair from CLI args.
fn arg_value(flag: &str) -> Option<String> {
    std::env::args()
        .skip_while(|a| a != flag)
        .nth(1)
}

#[tokio::main]
async fn main() {
    let remote = arg_value("--remote");

    if let Some(addr) = remote {
        run_remote(&addr).await;
    } else {
        run_local().await;
    }
}

// ---------------------------------------------------------------------------
// Remote mode: connect to a TronDB server via gRPC ExecuteTql
// ---------------------------------------------------------------------------

async fn run_remote(addr: &str) {
    let endpoint = if addr.starts_with("http") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    };

    println!("TronDB v0.2.0 — inference-first storage engine");
    println!("Connected to: {endpoint}");
    println!("Type .help for commands, or enter TQL statements ending with ;\n");

    let mut client = match trondb_proto::pb::tron_node_client::TronNodeClient::connect(
        endpoint.clone(),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to connect to {endpoint}: {e}");
            std::process::exit(1);
        }
    };

    let mut rl = DefaultEditor::new().expect("failed to create editor");
    let mut buffer = String::new();

    loop {
        let prompt = if buffer.is_empty() {
            "trondb> "
        } else {
            "   ...> "
        };

        match rl.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim();

                if buffer.is_empty() && trimmed.starts_with('.') {
                    handle_dot_command_remote(trimmed);
                    continue;
                }

                buffer.push_str(trimmed);
                buffer.push(' ');

                if !buffer.trim_end().ends_with(';') {
                    continue;
                }

                let input = buffer.trim().to_string();
                buffer.clear();

                let _ = rl.add_history_entry(&input);

                let request = trondb_proto::pb::TqlRequest { tql: input };
                match client.execute_tql(request).await {
                    Ok(response) => {
                        let resp = response.into_inner();
                        println!("{}", display::format_grpc_response(&resp));
                    }
                    Err(e) => eprintln!("Error: {e}"),
                }
            }
            Err(ReadlineError::Interrupted) => {
                buffer.clear();
                println!("(statement cleared)");
            }
            Err(ReadlineError::Eof) => {
                println!("Goodbye.");
                break;
            }
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        }
    }
}

fn handle_dot_command_remote(cmd: &str) {
    match cmd {
        ".help" => {
            println!("Commands:");
            println!("  .help          Show this help");
            println!("  .quit          Exit TronDB");
            println!();
            println!("TQL statements must end with a semicolon (;)");
            println!("(remote mode — .collections and .data not available)");
        }
        ".quit" | ".exit" => {
            println!("Goodbye.");
            std::process::exit(0);
        }
        _ => eprintln!("Unknown command: {cmd}. Type .help for available commands."),
    }
}

// ---------------------------------------------------------------------------
// Local mode: open a local engine (original behavior)
// ---------------------------------------------------------------------------

async fn run_local() {
    let data_dir = arg_value("--data-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./trondb_data"));

    let config = EngineConfig {
        data_dir: data_dir.join("store"),
        wal: WalConfig {
            wal_dir: data_dir.join("wal"),
            ..Default::default()
        },
        snapshot_interval_secs: 60,
        hnsw_snapshot_interval_secs: 300,
    };

    println!("TronDB v0.2.0 — inference-first storage engine");
    println!("Data directory: {}", data_dir.display());
    println!("Type .help for commands, or enter TQL statements ending with ;\n");

    let (engine, _pending_records) = match Engine::open(config).await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("Failed to open engine: {e}");
            std::process::exit(1);
        }
    };
    // Register vectorisers for existing collections and install a hook so
    // runtime CreateCollection / DropCollection also register/deregister vectorisers.
    register_vectorisers(&engine);

    let engine = Arc::new(engine);

    let wal = engine.wal_writer();
    let local_node = Arc::new(LocalNode::new(engine.clone(), NodeId::from_string("local")))
        as Arc<dyn trondb_routing::NodeHandle>;
    let mut router = SemanticRouter::with_wal(vec![local_node], RouterConfig::default(), Some(wal));

    let lru = Arc::new(std::sync::Mutex::new(trondb_routing::eviction::LruTracker::new()));
    let tier_config = TierConfig::default();
    let migrator = Arc::new(TierMigrator::new(
        tier_config,
        engine.clone(),
        lru,
        router.affinity_index().clone(),
    ));
    router.set_migrator(migrator);

    let sweeper = Arc::new(DecaySweeper::new(Arc::clone(&engine), 60));
    let _sweeper_handle = Arc::clone(&sweeper).spawn();

    let mut rl = DefaultEditor::new().expect("failed to create editor");
    let mut buffer = String::new();

    loop {
        let prompt = if buffer.is_empty() {
            "trondb> "
        } else {
            "   ...> "
        };

        match rl.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim();

                if buffer.is_empty() && trimmed.starts_with('.') {
                    handle_dot_command(trimmed, &engine, &data_dir);
                    continue;
                }

                buffer.push_str(trimmed);
                buffer.push(' ');

                if !buffer.trim_end().ends_with(';') {
                    continue;
                }

                let input = buffer.trim().to_string();
                buffer.clear();

                let _ = rl.add_history_entry(&input);

                match engine.parse_and_plan(&input) {
                    Ok(plan) => match router.route_and_execute(&plan).await {
                        Ok(result) => println!("{}", display::format_result(&result)),
                        Err(e) => eprintln!("Error: {e}"),
                    },
                    Err(e) => eprintln!("Error: {e}"),
                }
            }
            Err(ReadlineError::Interrupted) => {
                buffer.clear();
                println!("(statement cleared)");
            }
            Err(ReadlineError::Eof) => {
                println!("Goodbye.");
                break;
            }
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        }
    }
}

fn handle_dot_command(cmd: &str, engine: &Engine, data_dir: &std::path::Path) {
    match cmd {
        ".help" => {
            println!("Commands:");
            println!("  .help          Show this help");
            println!("  .collections   List all collections");
            println!("  .data          Show data directory");
            println!("  .quit          Exit TronDB");
            println!();
            println!("TQL statements must end with a semicolon (;)");
        }
        ".collections" => {
            let collections = engine.collections();
            if collections.is_empty() {
                println!("No collections.");
            } else {
                for name in collections {
                    println!("  {name}");
                }
            }
        }
        ".data" => {
            println!("Data directory: {}", data_dir.display());
        }
        ".quit" | ".exit" => {
            println!("Goodbye.");
            std::process::exit(0);
        }
        _ => eprintln!("Unknown command: {cmd}. Type .help for available commands."),
    }
}

/// Register vectorisers for all existing collections and install a lifecycle
/// hook so that runtime CreateCollection / DropCollection also
/// register/deregister vectorisers automatically.
fn register_vectorisers(engine: &Engine) {
    for schema in engine.schemas() {
        register_vectorisers_for_schema(engine.vectoriser_registry(), &schema);
    }

    let registry = Arc::clone(engine.vectoriser_registry());
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

fn register_vectorisers_for_schema(
    registry: &trondb_core::vectoriser::VectoriserRegistry,
    schema: &trondb_core::types::CollectionSchema,
) {
    if let Some(ref vc) = schema.vectoriser_config {
        for repr in &schema.representations {
            if !repr.fields.is_empty() {
                match trondb_vectoriser::create_vectoriser_from_config(vc, repr) {
                    Ok(v) => registry.register(&schema.name, &repr.name, v),
                    Err(e) => eprintln!(
                        "warning: could not create vectoriser for {}.{}: {e}",
                        schema.name, repr.name,
                    ),
                }
            }
        }
    }
}
