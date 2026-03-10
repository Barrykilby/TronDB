# TronDB

Inference-first storage engine. PoC phase: in-memory, single-node.

## Project Structure

- `crates/trondb-tql/` — TQL parser (logos lexer + recursive descent). No engine dependency.
- `crates/trondb-core/` — Engine: types, in-memory store, HNSW index, planner, executor. Depends on trondb-tql for AST types.
- `crates/trondb-cli/` — Interactive REPL binary. Depends on both.

## Conventions

- Rust 2021 edition, synchronous (no Tokio yet)
- Tests: `cargo test --workspace`
- Run REPL: `cargo run -p trondb-cli`
- TQL is case-insensitive for keywords, case-sensitive for identifiers
- All vectors are Float32 for now
- LogicalId is a String wrapper (user-provided or UUID v4)
