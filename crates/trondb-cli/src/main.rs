mod display;

use std::path::PathBuf;

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use trondb_core::{Engine, EngineConfig};
use trondb_wal::WalConfig;

#[tokio::main]
async fn main() {
    let data_dir = std::env::args()
        .skip_while(|a| a != "--data-dir")
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./trondb_data"));

    let config = EngineConfig {
        data_dir: data_dir.join("store"),
        wal: WalConfig {
            wal_dir: data_dir.join("wal"),
            ..Default::default()
        },
    };

    println!("TronDB v0.2.0 — inference-first storage engine");
    println!("Data directory: {}", data_dir.display());
    println!("Type .help for commands, or enter TQL statements ending with ;\n");

    let engine = match Engine::open(config).await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Failed to open engine: {e}");
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

                match engine.execute_tql(&input).await {
                    Ok(result) => println!("{}", display::format_result(&result)),
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
