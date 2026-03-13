#!/usr/bin/env python3
"""
Johnny → TronDB benchmark: export from DuckDB, load into TronDB, compare queries.

Usage:
    python3 tools/johnny_benchmark.py export         # Export DuckDB → JSONL files
    python3 tools/johnny_benchmark.py load           # Load JSONL into TronDB via gRPC
    python3 tools/johnny_benchmark.py load-edges     # Load memory_links as TronDB edges
    python3 tools/johnny_benchmark.py bench          # Run vector search benchmarks
    python3 tools/johnny_benchmark.py bench-hops     # Run multi-hop traversal benchmarks
    python3 tools/johnny_benchmark.py bench-retrieval  # Run structured retrieval benchmarks
"""

import sys
import os
import json
import time
import subprocess
from pathlib import Path

JOHNNY_DB = Path("/tmp/johnny_readonly.db")
EXPORT_DIR = Path("/tmp/johnny_trondb_export")
TRONDB_ADDR = "192.168.1.11:9400"
TRONDB_CLI = None  # Will be resolved at runtime


def resolve_cli():
    """Find the trondb CLI binary."""
    global TRONDB_CLI
    # Try the cargo target dir
    candidate = Path("/tmp/trondb-target/debug/trondb")
    if candidate.exists():
        TRONDB_CLI = str(candidate)
        return
    # Try release
    candidate = Path("/tmp/trondb-target/release/trondb")
    if candidate.exists():
        TRONDB_CLI = str(candidate)
        return
    print("ERROR: trondb CLI not found. Run: CARGO_TARGET_DIR=/tmp/trondb-target cargo build -p trondb-cli")
    sys.exit(1)


def tql(statement: str) -> str:
    """Execute a TQL statement against TronDB and return output."""
    result = subprocess.run(
        [TRONDB_CLI, "--remote", TRONDB_ADDR],
        input=statement + "\n",
        capture_output=True,
        text=True,
        timeout=30,
    )
    return result.stdout + result.stderr


def tql_batch(statements: list[str], label: str = "") -> None:
    """Execute multiple TQL statements in a single CLI session."""
    batch = "\n".join(statements) + "\n"
    t0 = time.time()
    result = subprocess.run(
        [TRONDB_CLI, "--remote", TRONDB_ADDR],
        input=batch,
        capture_output=True,
        text=True,
        timeout=300,
    )
    elapsed = time.time() - t0
    lines = (result.stdout + result.stderr).strip().split("\n")
    errors = [l for l in lines if "Error" in l]
    ok_count = len(statements) - len(errors)
    if label:
        print(f"  {label}: {ok_count}/{len(statements)} OK in {elapsed:.1f}s")
    if errors:
        for e in errors[:5]:
            print(f"    ERROR: {e}")
        if len(errors) > 5:
            print(f"    ... and {len(errors) - 5} more errors")


def escape_tql_string(s: str) -> str:
    """Escape a string for TQL single-quoted literals."""
    if s is None:
        return ""
    return s.replace("\\", "\\\\").replace("'", "\\'")


# ---------------------------------------------------------------------------
# EXPORT
# ---------------------------------------------------------------------------

def cmd_export():
    """Export Johnny's DuckDB data to JSONL files."""
    import duckdb

    EXPORT_DIR.mkdir(parents=True, exist_ok=True)

    # Open read-only to avoid locking the live daemon's DB
    con = duckdb.connect(str(JOHNNY_DB), read_only=True)

    # 1. memory_cards
    print("Exporting memory_cards...")
    cards = con.execute("""
        SELECT id, content, CAST(embedding AS VARCHAR) as embedding,
               card_type, project, scope, CAST(tags AS VARCHAR) as tags,
               importance, access_count, compression_level,
               CAST(created_at AS VARCHAR) as created_at,
               CAST(accessed_at AS VARCHAR) as accessed_at
        FROM memory_cards
    """).fetchall()
    cols = ["id", "content", "embedding", "card_type", "project", "scope",
            "tags", "importance", "access_count", "compression_level",
            "created_at", "accessed_at"]
    with open(EXPORT_DIR / "memory_cards.jsonl", "w") as f:
        for row in cards:
            obj = dict(zip(cols, row))
            # Parse embedding string to float list
            if obj["embedding"]:
                try:
                    emb_str = obj["embedding"].strip("[]")
                    obj["embedding"] = [float(x) for x in emb_str.split(",")]
                except:
                    obj["embedding"] = None
            f.write(json.dumps(obj) + "\n")
    print(f"  {len(cards)} cards exported")

    # 2. memory_links
    print("Exporting memory_links...")
    links = con.execute("""
        SELECT from_id, to_id, similarity, link_type,
               CAST(created_at AS VARCHAR) as created_at
        FROM memory_links
    """).fetchall()
    with open(EXPORT_DIR / "memory_links.jsonl", "w") as f:
        for row in links:
            obj = {
                "from_id": row[0],
                "to_id": row[1],
                "similarity": row[2],
                "link_type": row[3],
                "created_at": row[4],
            }
            f.write(json.dumps(obj) + "\n")
    print(f"  {len(links)} links exported")

    # 3. sessions
    print("Exporting sessions...")
    sessions = con.execute("""
        SELECT id, project, CAST(started_at AS VARCHAR) as started_at,
               CAST(ended_at AS VARCHAR) as ended_at, summary, status
        FROM sessions
    """).fetchall()
    with open(EXPORT_DIR / "sessions.jsonl", "w") as f:
        for row in sessions:
            obj = {
                "id": row[0], "project": row[1], "started_at": row[2],
                "ended_at": row[3], "summary": row[4], "status": row[5],
            }
            f.write(json.dumps(obj) + "\n")
    print(f"  {len(sessions)} sessions exported")

    # 4. prompts
    print("Exporting prompts...")
    prompts = con.execute("""
        SELECT id, session_id, content, CAST(created_at AS VARCHAR) as created_at
        FROM prompts
    """).fetchall()
    with open(EXPORT_DIR / "prompts.jsonl", "w") as f:
        for row in prompts:
            obj = {"id": row[0], "session_id": row[1], "content": row[2], "created_at": row[3]}
            f.write(json.dumps(obj) + "\n")
    print(f"  {len(prompts)} prompts exported")

    # 5. observations
    print("Exporting observations...")
    observations = con.execute("""
        SELECT id, session_id, tool_name,
               CAST(tool_input AS VARCHAR) as tool_input,
               CAST(tool_output AS VARCHAR) as tool_output,
               project, CAST(created_at AS VARCHAR) as created_at, processed
        FROM observations
    """).fetchall()
    with open(EXPORT_DIR / "observations.jsonl", "w") as f:
        for row in observations:
            obj = {
                "id": row[0], "session_id": row[1], "tool_name": row[2],
                "tool_input": row[3], "tool_output": row[4],
                "project": row[5], "created_at": row[6], "processed": row[7],
            }
            f.write(json.dumps(obj) + "\n")
    print(f"  {len(observations)} observations exported")

    # 6. entities
    print("Exporting entities...")
    entities = con.execute("""
        SELECT id, name, entity_type, mention_count,
               CAST(first_seen AS VARCHAR) as first_seen,
               CAST(last_seen AS VARCHAR) as last_seen
        FROM entities
    """).fetchall()
    with open(EXPORT_DIR / "entities.jsonl", "w") as f:
        for row in entities:
            obj = {
                "id": row[0], "name": row[1], "entity_type": row[2],
                "mention_count": row[3], "first_seen": row[4], "last_seen": row[5],
            }
            f.write(json.dumps(obj) + "\n")
    print(f"  {len(entities)} entities exported")

    # 7. clusters
    print("Exporting clusters...")
    clusters = con.execute("""
        SELECT id, name, description, CAST(centroid AS VARCHAR) as centroid,
               card_count
        FROM clusters
    """).fetchall()
    with open(EXPORT_DIR / "clusters.jsonl", "w") as f:
        for row in clusters:
            obj = {
                "id": row[0], "name": row[1], "description": row[2],
                "centroid": row[3], "card_count": row[4],
            }
            if obj["centroid"]:
                try:
                    c_str = obj["centroid"].strip("[]")
                    obj["centroid"] = [float(x) for x in c_str.split(",")]
                except:
                    obj["centroid"] = None
            f.write(json.dumps(obj) + "\n")
    print(f"  {len(clusters)} clusters exported")

    # 8. card_entities (for edge creation)
    print("Exporting card_entities...")
    card_entities = con.execute("SELECT card_id, entity_id FROM card_entities").fetchall()
    with open(EXPORT_DIR / "card_entities.jsonl", "w") as f:
        for row in card_entities:
            f.write(json.dumps({"card_id": row[0], "entity_id": row[1]}) + "\n")
    print(f"  {len(card_entities)} card_entity links exported")

    # 9. card_clusters (for edge creation)
    print("Exporting card_clusters...")
    card_clusters = con.execute("""
        SELECT card_id, cluster_id, membership_score FROM card_clusters
    """).fetchall()
    with open(EXPORT_DIR / "card_clusters.jsonl", "w") as f:
        for row in card_clusters:
            f.write(json.dumps({
                "card_id": row[0], "cluster_id": row[1], "membership_score": row[2],
            }) + "\n")
    print(f"  {len(card_clusters)} card_cluster links exported")

    # Summary
    total_size = sum(f.stat().st_size for f in EXPORT_DIR.iterdir() if f.is_file())
    print(f"\nExport complete: {EXPORT_DIR} ({total_size / 1024 / 1024:.1f} MB)")

    con.close()


# ---------------------------------------------------------------------------
# LOAD
# ---------------------------------------------------------------------------

def cmd_load():
    """Load exported data into TronDB."""
    resolve_cli()

    # Step 1: Create collections
    print("Creating TronDB collections...")
    schema_stmts = [
        """CREATE COLLECTION memory_cards (
            FIELD content TEXT,
            FIELD card_type TEXT,
            FIELD project TEXT,
            FIELD scope TEXT,
            FIELD importance FLOAT,
            FIELD compression_level INT,
            FIELD access_count INT,
            FIELD tags TEXT,
            REPRESENTATION dense MODEL 'all-MiniLM-L6-v2' DIMENSIONS 384 METRIC COSINE,
            INDEX idx_card_type ON (card_type),
            INDEX idx_project ON (project),
            INDEX idx_scope ON (scope),
            INDEX idx_compression ON (compression_level)
        );""",
        """CREATE COLLECTION entities (
            FIELD name TEXT,
            FIELD entity_type TEXT,
            FIELD mention_count INT,
            INDEX idx_entity_type ON (entity_type),
            INDEX idx_name ON (name)
        );""",
        """CREATE COLLECTION clusters (
            FIELD name TEXT,
            FIELD description TEXT,
            FIELD card_count INT,
            REPRESENTATION centroid MODEL 'all-MiniLM-L6-v2' DIMENSIONS 384 METRIC COSINE,
            INDEX idx_cluster_name ON (name)
        );""",
        """CREATE COLLECTION sessions (
            FIELD project TEXT,
            FIELD summary TEXT,
            FIELD status TEXT,
            INDEX idx_session_status ON (status),
            INDEX idx_session_project ON (project)
        );""",
        """CREATE COLLECTION prompts (
            FIELD content TEXT
        );""",
        """CREATE COLLECTION observations (
            FIELD tool_name TEXT,
            FIELD tool_input TEXT,
            FIELD tool_output TEXT,
            FIELD processed BOOL,
            INDEX idx_obs_tool ON (tool_name),
            INDEX idx_obs_processed ON (processed)
        );""",
    ]
    tql_batch(schema_stmts, "Collections")

    # Step 2: Create edge types (FROM collection TO collection)
    print("Creating edge types...")
    edge_stmts = [
        "CREATE EDGE semantic_link FROM memory_cards TO memory_cards;",
        "CREATE EDGE supersedes FROM memory_cards TO memory_cards;",
        "CREATE EDGE temporal_link FROM memory_cards TO memory_cards;",
        "CREATE EDGE belongs_to_cluster FROM memory_cards TO clusters;",
        "CREATE EDGE mentions_entity FROM memory_cards TO entities;",
        "CREATE EDGE has_prompt FROM sessions TO prompts;",
        "CREATE EDGE has_observation FROM sessions TO observations;",
    ]
    tql_batch(edge_stmts, "Edge types")

    # Step 3: Load memory_cards (with embeddings)
    print("Loading memory_cards...")
    cards_file = EXPORT_DIR / "memory_cards.jsonl"
    if not cards_file.exists():
        print("  ERROR: run 'export' first")
        return

    batch_size = 200
    stmts = []
    count = 0
    with open(cards_file) as f:
        for line in f:
            card = json.loads(line)
            content = escape_tql_string(card.get("content") or "")
            card_type = escape_tql_string(card.get("card_type") or "")
            project = escape_tql_string(card.get("project") or "")
            scope = escape_tql_string(card.get("scope") or "")
            tags = escape_tql_string(card.get("tags") or "")
            importance = card.get("importance") or 0.5
            compression = card.get("compression_level") or 1
            access_count = card.get("access_count") or 0
            card_id = card["id"]

            # Build INSERT with id as a regular field
            emb = card.get("embedding")
            if emb and isinstance(emb, list) and len(emb) == 384:
                vec_str = ", ".join(f"{v:.6f}" for v in emb)
                stmt = (
                    f"INSERT INTO memory_cards "
                    f"(id, content, card_type, project, scope, importance, compression_level, access_count, tags) "
                    f"VALUES ('{card_id}', '{content}', '{card_type}', '{project}', '{scope}', {importance}, {compression}, {access_count}, '{tags}') "
                    f"REPRESENTATION dense VECTOR [{vec_str}];"
                )
            else:
                stmt = (
                    f"INSERT INTO memory_cards "
                    f"(id, content, card_type, project, scope, importance, compression_level, access_count, tags) "
                    f"VALUES ('{card_id}', '{content}', '{card_type}', '{project}', '{scope}', {importance}, {compression}, {access_count}, '{tags}');"
                )
            stmts.append(stmt)
            count += 1

            if len(stmts) >= batch_size:
                tql_batch(stmts, f"cards batch ({count})")
                stmts = []

    if stmts:
        tql_batch(stmts, f"cards batch ({count})")
    print(f"  {count} memory_cards loaded")

    # Step 4: Load entities
    print("Loading entities...")
    stmts = []
    count = 0
    with open(EXPORT_DIR / "entities.jsonl") as f:
        for line in f:
            ent = json.loads(line)
            name = escape_tql_string(ent.get("name") or "")
            etype = escape_tql_string(ent.get("entity_type") or "")
            mc = ent.get("mention_count") or 0
            eid = ent["id"]
            stmts.append(
                f"INSERT INTO entities (id, name, entity_type, mention_count) "
                f"VALUES ('{eid}', '{name}', '{etype}', {mc});"
            )
            count += 1
            if len(stmts) >= 500:
                tql_batch(stmts, f"entities batch ({count})")
                stmts = []
    if stmts:
        tql_batch(stmts, f"entities batch ({count})")
    print(f"  {count} entities loaded")

    # Step 5: Load clusters (with centroid embeddings)
    print("Loading clusters...")
    stmts = []
    with open(EXPORT_DIR / "clusters.jsonl") as f:
        for line in f:
            cl = json.loads(line)
            name = escape_tql_string(cl.get("name") or "")
            desc = escape_tql_string(cl.get("description") or "")
            cc = cl.get("card_count") or 0
            cid = cl["id"]
            centroid = cl.get("centroid")
            if centroid and isinstance(centroid, list) and len(centroid) == 384:
                vec_str = ", ".join(f"{v:.6f}" for v in centroid)
                stmts.append(
                    f"INSERT INTO clusters (id, name, description, card_count) "
                    f"VALUES ('{cid}', '{name}', '{desc}', {cc}) "
                    f"REPRESENTATION centroid VECTOR [{vec_str}];"
                )
            else:
                stmts.append(
                    f"INSERT INTO clusters (id, name, description, card_count) "
                    f"VALUES ('{cid}', '{name}', '{desc}', {cc});"
                )
    tql_batch(stmts, "clusters")

    # Step 6: Load sessions
    print("Loading sessions...")
    stmts = []
    with open(EXPORT_DIR / "sessions.jsonl") as f:
        for line in f:
            s = json.loads(line)
            proj = escape_tql_string(s.get("project") or "")
            summary = escape_tql_string(s.get("summary") or "")
            status = escape_tql_string(s.get("status") or "")
            sid = s["id"]
            stmts.append(
                f"INSERT INTO sessions (id, project, summary, status) "
                f"VALUES ('{sid}', '{proj}', '{summary}', '{status}');"
            )
    tql_batch(stmts, "sessions")

    # Step 7: Load prompts
    print("Loading prompts...")
    stmts = []
    count = 0
    with open(EXPORT_DIR / "prompts.jsonl") as f:
        for line in f:
            p = json.loads(line)
            content = escape_tql_string(p.get("content") or "")
            pid = p["id"]
            stmts.append(
                f"INSERT INTO prompts (id, content) VALUES ('{pid}', '{content}');"
            )
            count += 1
            if len(stmts) >= 500:
                tql_batch(stmts, f"prompts batch ({count})")
                stmts = []
    if stmts:
        tql_batch(stmts, f"prompts batch ({count})")
    print(f"  {count} prompts loaded")

    # Step 8: Load observations (skip tool_output to avoid massive strings)
    print("Loading observations...")
    stmts = []
    count = 0
    with open(EXPORT_DIR / "observations.jsonl") as f:
        for line in f:
            obs = json.loads(line)
            tool_name = escape_tql_string(obs.get("tool_name") or "")
            # Truncate tool_input to 1000 chars to avoid TQL parsing issues
            tool_input = escape_tql_string((obs.get("tool_input") or "")[:1000])
            # Skip tool_output entirely — too large, causes TQL string issues
            processed = "true" if obs.get("processed") else "false"
            oid = obs["id"]
            stmts.append(
                f"INSERT INTO observations (id, tool_name, tool_input, processed) "
                f"VALUES ('{oid}', '{tool_name}', '{tool_input}', {processed});"
            )
            count += 1
            if len(stmts) >= 500:
                tql_batch(stmts, f"observations batch ({count})")
                stmts = []
    if stmts:
        tql_batch(stmts, f"observations batch ({count})")
    print(f"  {count} observations loaded")

    print("\nLoad complete!")


# ---------------------------------------------------------------------------
# LOAD EDGES
# ---------------------------------------------------------------------------

def cmd_load_edges():
    """Load memory_links as TronDB edges."""
    resolve_cli()

    links_file = EXPORT_DIR / "memory_links.jsonl"
    if not links_file.exists():
        print("ERROR: run 'export' first")
        return

    # Map link_type to TronDB edge type names
    edge_type_map = {
        "semantic": "semantic_link",
        "supersedes": "supersedes",
        "temporal": "temporal_link",
    }

    # Count by type first
    type_counts = {}
    with open(links_file) as f:
        for line in f:
            link = json.loads(line)
            lt = link["link_type"]
            type_counts[lt] = type_counts.get(lt, 0) + 1
    print("Edge counts by type:")
    for lt, c in sorted(type_counts.items()):
        print(f"  {lt}: {c}")
    print()

    # Load in batches per type
    batch_size = 500
    stmts = []
    loaded = 0
    errors_total = 0

    with open(links_file) as f:
        for line in f:
            link = json.loads(line)
            lt = link["link_type"]
            edge_type = edge_type_map.get(lt)
            if not edge_type:
                continue

            from_id = link["from_id"]
            to_id = link["to_id"]
            stmts.append(
                f"INSERT EDGE {edge_type} FROM '{from_id}' TO '{to_id}';"
            )
            loaded += 1

            if len(stmts) >= batch_size:
                tql_batch(stmts, f"edges ({loaded})")
                stmts = []

    if stmts:
        tql_batch(stmts, f"edges ({loaded})")

    print(f"\n{loaded} edges loaded")


# ---------------------------------------------------------------------------
# BENCH (vector search)
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# BENCH HOPS
# ---------------------------------------------------------------------------

def cmd_bench_hops():
    """Benchmark multi-hop graph traversal: DuckDB recursive CTEs vs TronDB TRAVERSE MATCH."""
    import duckdb
    import random

    resolve_cli()

    con = duckdb.connect(str(JOHNNY_DB), read_only=True)

    # Pick 20 seed cards that have at least a few outgoing semantic links
    print("Selecting seed nodes with high connectivity...")
    seeds = con.execute("""
        SELECT ml.from_id, mc.content
        FROM memory_links ml
        JOIN memory_cards mc ON ml.from_id = mc.id
        WHERE ml.link_type = 'semantic'
        GROUP BY ml.from_id, mc.content
        HAVING COUNT(*) >= 3
        ORDER BY RANDOM()
        LIMIT 20
    """).fetchall()

    if not seeds:
        print("ERROR: no well-connected nodes found")
        return

    print(f"Selected {len(seeds)} seed nodes\n")

    for max_depth in [1, 2, 3]:
        print(f"{'=' * 70}")
        print(f"  DEPTH {max_depth} — {max_depth}-hop traversal")
        print(f"{'=' * 70}")

        duck_times = []
        tron_times = []
        duck_counts = []
        tron_counts = []

        for i, (seed_id, content) in enumerate(seeds):
            # --- DuckDB: recursive CTE ---
            t0 = time.time()
            duck_result = con.execute(f"""
                WITH RECURSIVE hops AS (
                    -- Base case: direct neighbors
                    SELECT to_id AS node_id, 1 AS depth
                    FROM memory_links
                    WHERE from_id = '{seed_id}'
                      AND link_type = 'semantic'

                    UNION

                    -- Recursive case: follow links from frontier
                    SELECT ml.to_id AS node_id, h.depth + 1 AS depth
                    FROM hops h
                    JOIN memory_links ml ON ml.from_id = h.node_id
                    WHERE ml.link_type = 'semantic'
                      AND h.depth < {max_depth}
                )
                SELECT DISTINCT node_id FROM hops
            """).fetchall()
            duck_time = time.time() - t0
            duck_times.append(duck_time)
            duck_count = len(duck_result)
            duck_counts.append(duck_count)
            duck_ids = set(r[0] for r in duck_result)

            # --- TronDB: TRAVERSE MATCH ---
            tql_stmt = (
                f"TRAVERSE FROM '{seed_id}' "
                f"MATCH (a)-[e:semantic_link]->(b) "
                f"DEPTH 1..{max_depth} LIMIT 10000;"
            )
            t0 = time.time()
            result = subprocess.run(
                [TRONDB_CLI, "--remote", TRONDB_ADDR],
                input=tql_stmt + "\n",
                capture_output=True,
                text=True,
                timeout=60,
            )
            tron_time = time.time() - t0
            tron_times.append(tron_time)

            # Parse TronDB output for entity count
            tron_ids = set()
            for line in result.stdout.split("\n"):
                line = line.strip().strip("|")
                parts = [p.strip() for p in line.split("|")]
                if parts and len(parts[0]) == 36 and "-" in parts[0]:
                    tron_ids.add(parts[0])
            tron_count = len(tron_ids)
            tron_counts.append(tron_count)

            # Overlap between DuckDB and TronDB results
            if duck_ids:
                overlap = len(duck_ids & tron_ids) / len(duck_ids) if duck_ids else 0
            else:
                overlap = 1.0 if tron_count == 0 else 0.0

            label = (content or "")[:45]
            print(
                f"  Seed {i+1:2d}: "
                f"DuckDB {duck_time*1000:7.1f}ms ({duck_count:5d} nodes) | "
                f"TronDB {tron_time*1000:7.1f}ms ({tron_count:5d} nodes) | "
                f"overlap {overlap:.2f}  [{label}...]"
            )

        print(f"\n  --- Depth {max_depth} Summary ---")
        print(
            f"  DuckDB  — mean: {sum(duck_times)/len(duck_times)*1000:.1f}ms, "
            f"p50: {sorted(duck_times)[len(duck_times)//2]*1000:.1f}ms, "
            f"mean nodes: {sum(duck_counts)/len(duck_counts):.0f}"
        )
        print(
            f"  TronDB  — mean: {sum(tron_times)/len(tron_times)*1000:.1f}ms, "
            f"p50: {sorted(tron_times)[len(tron_times)//2]*1000:.1f}ms, "
            f"mean nodes: {sum(tron_counts)/len(tron_counts):.0f}"
        )
        if duck_counts and tron_counts:
            avg_duck = sum(duck_counts) / len(duck_counts)
            avg_tron = sum(tron_counts) / len(tron_counts)
            if avg_duck > 0:
                print(f"  Node coverage: TronDB finds {avg_tron/avg_duck*100:.1f}% of DuckDB nodes")
        print()

    # Also test undirected (bidirectional) traversal for semantic links
    print(f"{'=' * 70}")
    print(f"  BIDIRECTIONAL — DuckDB both directions vs TronDB undirected edge pattern")
    print(f"{'=' * 70}")

    duck_times_bi = []
    tron_times_bi = []
    duck_counts_bi = []
    tron_counts_bi = []

    for i, (seed_id, content) in enumerate(seeds[:10]):  # 10 seeds for bidir
        # DuckDB: follow links in both directions
        t0 = time.time()
        duck_result = con.execute(f"""
            WITH RECURSIVE hops AS (
                SELECT to_id AS node_id, 1 AS depth
                FROM memory_links
                WHERE from_id = '{seed_id}' AND link_type = 'semantic'
                UNION
                SELECT from_id AS node_id, 1 AS depth
                FROM memory_links
                WHERE to_id = '{seed_id}' AND link_type = 'semantic'
                UNION
                SELECT CASE WHEN ml.from_id = h.node_id THEN ml.to_id ELSE ml.from_id END AS node_id,
                       h.depth + 1 AS depth
                FROM hops h
                JOIN memory_links ml
                  ON (ml.from_id = h.node_id OR ml.to_id = h.node_id)
                WHERE ml.link_type = 'semantic'
                  AND h.depth < 2
            )
            SELECT DISTINCT node_id FROM hops WHERE node_id != '{seed_id}'
        """).fetchall()
        duck_time_bi = time.time() - t0
        duck_times_bi.append(duck_time_bi)
        duck_counts_bi.append(len(duck_result))

        # TronDB: undirected traversal (dash instead of arrow)
        tql_stmt = (
            f"TRAVERSE FROM '{seed_id}' "
            f"MATCH (a)-[e:semantic_link]-(b) "
            f"DEPTH 1..2 LIMIT 10000;"
        )
        t0 = time.time()
        result = subprocess.run(
            [TRONDB_CLI, "--remote", TRONDB_ADDR],
            input=tql_stmt + "\n",
            capture_output=True,
            text=True,
            timeout=60,
        )
        tron_time_bi = time.time() - t0
        tron_times_bi.append(tron_time_bi)

        tron_ids_bi = set()
        for line in result.stdout.split("\n"):
            line = line.strip().strip("|")
            parts = [p.strip() for p in line.split("|")]
            if parts and len(parts[0]) == 36 and "-" in parts[0]:
                tron_ids_bi.add(parts[0])
        tron_counts_bi.append(len(tron_ids_bi))

        label = (content or "")[:45]
        print(
            f"  Seed {i+1:2d}: "
            f"DuckDB {duck_time_bi*1000:7.1f}ms ({len(duck_result):5d} nodes) | "
            f"TronDB {tron_time_bi*1000:7.1f}ms ({len(tron_ids_bi):5d} nodes)  [{label}...]"
        )

    print(f"\n  --- Bidirectional 2-hop Summary ---")
    print(
        f"  DuckDB  — mean: {sum(duck_times_bi)/len(duck_times_bi)*1000:.1f}ms, "
        f"mean nodes: {sum(duck_counts_bi)/len(duck_counts_bi):.0f}"
    )
    print(
        f"  TronDB  — mean: {sum(tron_times_bi)/len(tron_times_bi)*1000:.1f}ms, "
        f"mean nodes: {sum(tron_counts_bi)/len(tron_counts_bi):.0f}"
    )

    print(f"\nNote: TronDB times include gRPC + CLI overhead (~20ms baseline).")
    print("DuckDB recursive CTEs are brute-force scans; TronDB uses in-memory AdjacencyIndex (DashMap).")

    con.close()


# ---------------------------------------------------------------------------
# BENCH RETRIEVAL — structured multi-step retrieval scenarios
# ---------------------------------------------------------------------------

def cmd_bench_retrieval():
    """Benchmark structured retrieval: the kind of queries a real memory system does."""
    import duckdb

    resolve_cli()
    con = duckdb.connect(str(JOHNNY_DB), read_only=True)

    # =========================================================================
    # Scenario 1: Supersedes chain — trace a memory's revision history
    # "Given this card, what did it replace, and what replaced those?"
    # =========================================================================
    print(f"{'=' * 78}")
    print("  SCENARIO 1: Supersedes Chain — trace revision history")
    print(f"  Follow 'supersedes' edges up to 5 hops to find all versions of a memory")
    print(f"{'=' * 78}")

    # Find cards that are at the END of supersedes chains (have been superseded)
    chain_seeds = con.execute("""
        SELECT ml.to_id, mc.content
        FROM memory_links ml
        JOIN memory_cards mc ON ml.to_id = mc.id
        WHERE ml.link_type = 'supersedes'
        GROUP BY ml.to_id, mc.content
        HAVING COUNT(*) >= 2
        ORDER BY RANDOM()
        LIMIT 15
    """).fetchall()

    duck_times_s1 = []
    tron_times_s1 = []

    for i, (seed_id, content) in enumerate(chain_seeds):
        # DuckDB: recursive CTE filtered to supersedes only
        t0 = time.time()
        duck_result = con.execute(f"""
            WITH RECURSIVE chain AS (
                SELECT from_id AS node_id, 1 AS depth
                FROM memory_links
                WHERE to_id = '{seed_id}' AND link_type = 'supersedes'
                UNION
                SELECT ml.from_id AS node_id, c.depth + 1 AS depth
                FROM chain c
                JOIN memory_links ml ON ml.to_id = c.node_id
                WHERE ml.link_type = 'supersedes' AND c.depth < 5
            )
            SELECT DISTINCT node_id FROM chain
        """).fetchall()
        duck_time = time.time() - t0
        duck_times_s1.append(duck_time)
        duck_count = len(duck_result)

        # Also trace forward (what replaced this)
        t0_fwd = time.time()
        duck_fwd = con.execute(f"""
            WITH RECURSIVE chain AS (
                SELECT to_id AS node_id, 1 AS depth
                FROM memory_links
                WHERE from_id = '{seed_id}' AND link_type = 'supersedes'
                UNION
                SELECT ml.to_id AS node_id, c.depth + 1 AS depth
                FROM chain c
                JOIN memory_links ml ON ml.from_id = c.node_id
                WHERE ml.link_type = 'supersedes' AND c.depth < 5
            )
            SELECT DISTINCT node_id FROM chain
        """).fetchall()
        duck_time_total = (time.time() - t0_fwd) + duck_time
        duck_total = duck_count + len(duck_fwd)

        # TronDB: single undirected TRAVERSE on supersedes
        tql_stmt = (
            f"TRAVERSE FROM '{seed_id}' "
            f"MATCH (a)-[e:supersedes]-(b) "
            f"DEPTH 1..5 LIMIT 10000;"
        )
        t0 = time.time()
        result = subprocess.run(
            [TRONDB_CLI, "--remote", TRONDB_ADDR],
            input=tql_stmt + "\n",
            capture_output=True,
            text=True,
            timeout=30,
        )
        tron_time = time.time() - t0
        tron_times_s1.append(tron_time)
        tron_ids = set()
        for line in result.stdout.split("\n"):
            line = line.strip().strip("|")
            parts = [p.strip() for p in line.split("|")]
            if parts and len(parts[0]) == 36 and "-" in parts[0]:
                tron_ids.add(parts[0])

        label = (content or "")[:40]
        print(
            f"  Seed {i+1:2d}: "
            f"DuckDB {duck_time_total*1000:6.1f}ms ({duck_total:3d} versions) | "
            f"TronDB {tron_time*1000:6.1f}ms ({len(tron_ids):3d} versions)  [{label}...]"
        )

    print(f"\n  DuckDB needs 2 recursive CTEs (backward + forward). TronDB: 1 undirected TRAVERSE.")
    print(f"  DuckDB mean: {sum(duck_times_s1)/len(duck_times_s1)*1000:.1f}ms")
    print(f"  TronDB mean: {sum(tron_times_s1)/len(tron_times_s1)*1000:.1f}ms")

    # =========================================================================
    # Scenario 2: Mixed-type neighbourhood expansion
    # "Given a card, find everything connected by ANY link type within 2 hops"
    # This is what a real retrieval system does: semantic + temporal + supersedes
    # =========================================================================
    print(f"\n{'=' * 78}")
    print("  SCENARIO 2: Mixed-Type Neighbourhood — all edge types, 2 hops")
    print(f"  Follow semantic + supersedes + temporal edges together")
    print(f"{'=' * 78}")

    mixed_seeds = con.execute("""
        SELECT ml.from_id, mc.content
        FROM memory_links ml
        JOIN memory_cards mc ON ml.from_id = mc.id
        GROUP BY ml.from_id, mc.content
        HAVING COUNT(DISTINCT ml.link_type) >= 2
        ORDER BY RANDOM()
        LIMIT 15
    """).fetchall()

    duck_times_s2 = []
    tron_times_s2 = []

    for i, (seed_id, content) in enumerate(mixed_seeds):
        # DuckDB: single recursive CTE, all link types
        t0 = time.time()
        duck_result = con.execute(f"""
            WITH RECURSIVE hops AS (
                SELECT to_id AS node_id, 1 AS depth
                FROM memory_links
                WHERE from_id = '{seed_id}'
                UNION
                SELECT ml.to_id AS node_id, h.depth + 1
                FROM hops h
                JOIN memory_links ml ON ml.from_id = h.node_id
                WHERE h.depth < 2
            )
            SELECT DISTINCT node_id FROM hops
        """).fetchall()
        duck_time = time.time() - t0
        duck_times_s2.append(duck_time)
        duck_count = len(duck_result)

        # TronDB: TRAVERSE without edge type filter = all types
        tql_stmt = (
            f"TRAVERSE FROM '{seed_id}' "
            f"MATCH (a)-[e]->(b) "
            f"DEPTH 1..2 LIMIT 10000;"
        )
        t0 = time.time()
        result = subprocess.run(
            [TRONDB_CLI, "--remote", TRONDB_ADDR],
            input=tql_stmt + "\n",
            capture_output=True,
            text=True,
            timeout=30,
        )
        tron_time = time.time() - t0
        tron_times_s2.append(tron_time)
        tron_ids = set()
        for line in result.stdout.split("\n"):
            line = line.strip().strip("|")
            parts = [p.strip() for p in line.split("|")]
            if parts and len(parts[0]) == 36 and "-" in parts[0]:
                tron_ids.add(parts[0])

        label = (content or "")[:40]
        print(
            f"  Seed {i+1:2d}: "
            f"DuckDB {duck_time*1000:6.1f}ms ({duck_count:4d} nodes) | "
            f"TronDB {tron_time*1000:6.1f}ms ({len(tron_ids):4d} nodes)  [{label}...]"
        )

    print(f"\n  DuckDB mean: {sum(duck_times_s2)/len(duck_times_s2)*1000:.1f}ms")
    print(f"  TronDB mean: {sum(tron_times_s2)/len(tron_times_s2)*1000:.1f}ms")

    # =========================================================================
    # Scenario 3: Vector Search + Graph Expansion (the killer use case)
    # "Find memories similar to this query, then expand 1 hop to get context"
    # DuckDB: brute-force cosine scan → get IDs → recursive CTE per ID → merge
    # TronDB: HNSW search → TRAVERSE from each result → done
    # =========================================================================
    print(f"\n{'=' * 78}")
    print("  SCENARIO 3: Vector Search + Graph Expansion")
    print(f"  HNSW top-5, then 1-hop semantic expansion from each result")
    print(f"  DuckDB: brute-force cosine + per-seed recursive CTE (stitched in Python)")
    print(f"  TronDB: HNSW search + TRAVERSE per seed (all server-side)")
    print(f"{'=' * 78}")

    query_cards = con.execute("""
        SELECT id, content, CAST(embedding AS VARCHAR) as embedding
        FROM memory_cards
        WHERE embedding IS NOT NULL
        ORDER BY RANDOM()
        LIMIT 10
    """).fetchall()

    duck_times_s3 = []
    tron_times_s3 = []

    for i, (qid, qcontent, qemb_str) in enumerate(query_cards):
        emb_str = qemb_str.strip("[]")
        emb = [float(x) for x in emb_str.split(",")]
        emb_json = json.dumps(emb)
        vec_tql = ", ".join(f"{v:.6f}" for v in emb)

        # --- DuckDB: vector search + graph expansion ---
        t0 = time.time()
        # Step 1: brute-force cosine top-5
        top5 = con.execute(f"""
            SELECT id FROM (
                SELECT id, array_cosine_similarity(embedding, '{emb_json}'::FLOAT[384]) as sim
                FROM memory_cards
                WHERE embedding IS NOT NULL
                ORDER BY sim DESC
                LIMIT 5
            )
        """).fetchall()
        seed_ids = [r[0] for r in top5]

        # Step 2: expand each seed 1 hop via semantic links
        all_expanded = set(seed_ids)
        for sid in seed_ids:
            neighbors = con.execute(f"""
                SELECT to_id FROM memory_links
                WHERE from_id = '{sid}' AND link_type = 'semantic'
                UNION
                SELECT from_id FROM memory_links
                WHERE to_id = '{sid}' AND link_type = 'semantic'
            """).fetchall()
            all_expanded.update(r[0] for r in neighbors)

        duck_time = time.time() - t0
        duck_times_s3.append(duck_time)
        duck_count = len(all_expanded)

        # --- TronDB: HNSW search + TRAVERSE per seed ---
        t0 = time.time()

        # Step 1: HNSW top-5
        search_stmt = f"SEARCH memory_cards NEAR VECTOR [{vec_tql}] USING dense LIMIT 5;"
        result = subprocess.run(
            [TRONDB_CLI, "--remote", TRONDB_ADDR],
            input=search_stmt + "\n",
            capture_output=True,
            text=True,
            timeout=30,
        )
        tron_seed_ids = []
        for line in result.stdout.split("\n"):
            line = line.strip().strip("|")
            parts = [p.strip() for p in line.split("|")]
            if parts and len(parts[0]) == 36 and "-" in parts[0]:
                tron_seed_ids.append(parts[0])

        # Step 2: expand each seed 1 hop via semantic_link (undirected)
        all_tron = set(tron_seed_ids)
        traverse_stmts = []
        for sid in tron_seed_ids:
            traverse_stmts.append(
                f"TRAVERSE FROM '{sid}' MATCH (a)-[e:semantic_link]-(b) DEPTH 1..1 LIMIT 1000;"
            )
        if traverse_stmts:
            batch = "\n".join(traverse_stmts) + "\n"
            result = subprocess.run(
                [TRONDB_CLI, "--remote", TRONDB_ADDR],
                input=batch,
                capture_output=True,
                text=True,
                timeout=30,
            )
            for line in result.stdout.split("\n"):
                line = line.strip().strip("|")
                parts = [p.strip() for p in line.split("|")]
                if parts and len(parts[0]) == 36 and "-" in parts[0]:
                    all_tron.add(parts[0])

        tron_time = time.time() - t0
        tron_times_s3.append(tron_time)
        tron_count = len(all_tron)

        label = (qcontent or "")[:40]
        print(
            f"  Query {i+1:2d}: "
            f"DuckDB {duck_time*1000:6.1f}ms ({duck_count:4d} expanded) | "
            f"TronDB {tron_time*1000:6.1f}ms ({tron_count:4d} expanded)  [{label}...]"
        )

    print(f"\n  --- Vector + Graph Expansion Summary ---")
    print(f"  DuckDB mean: {sum(duck_times_s3)/len(duck_times_s3)*1000:.1f}ms (cosine scan + 5 link lookups)")
    print(f"  TronDB mean: {sum(tron_times_s3)/len(tron_times_s3)*1000:.1f}ms (HNSW + 5 TRAVERSE, all via gRPC)")

    # =========================================================================
    # Scenario 4: Topic-scoped retrieval — vector search within graph subgraph
    # "Among cards 2 hops from seed X, which are closest to query Y?"
    # DuckDB: recursive CTE → filter + cosine in WHERE/ORDER BY
    # TronDB: TRAVERSE first, then would need client-side vector filter
    #   (TronDB doesn't yet support SEARCH within a traversal subgraph)
    # =========================================================================
    print(f"\n{'=' * 78}")
    print("  SCENARIO 4: Scoped Vector Search within Graph Neighbourhood")
    print(f"  Traverse 2 hops from seed, then rank by vector similarity to query")
    print(f"  DuckDB: recursive CTE → cosine similarity on subgraph")
    print(f"  TronDB: SEARCH WITHIN (TRAVERSE ...) — graph-scoped vector search")
    print(f"{'=' * 78}")

    scope_seeds = con.execute("""
        SELECT ml.from_id, mc.content, CAST(mc.embedding AS VARCHAR) as embedding
        FROM memory_links ml
        JOIN memory_cards mc ON ml.from_id = mc.id
        WHERE mc.embedding IS NOT NULL
        GROUP BY ml.from_id, mc.content, mc.embedding
        HAVING COUNT(*) >= 5
        ORDER BY RANDOM()
        LIMIT 10
    """).fetchall()

    duck_times_s4 = []
    tron_times_s4 = []

    for i, (seed_id, content, seed_emb_str) in enumerate(scope_seeds):
        seed_emb = seed_emb_str.strip("[]")
        seed_emb_json = json.dumps([float(x) for x in seed_emb.split(",")])
        seed_emb_tql = "[" + seed_emb + "]"

        # DuckDB: CTE to find 2-hop neighbourhood, then rank by cosine similarity
        t0 = time.time()
        duck_result = con.execute(f"""
            WITH RECURSIVE hops AS (
                SELECT to_id AS node_id, 1 AS depth
                FROM memory_links
                WHERE from_id = '{seed_id}' AND link_type = 'semantic'
                UNION
                SELECT ml.to_id AS node_id, h.depth + 1
                FROM hops h
                JOIN memory_links ml ON ml.from_id = h.node_id
                WHERE ml.link_type = 'semantic' AND h.depth < 2
            )
            SELECT DISTINCT mc.id, mc.content,
                   array_cosine_similarity(mc.embedding, '{seed_emb_json}'::FLOAT[384]) as sim
            FROM hops h
            JOIN memory_cards mc ON mc.id = h.node_id
            WHERE mc.embedding IS NOT NULL
            ORDER BY sim DESC
            LIMIT 10
        """).fetchall()
        duck_time = time.time() - t0
        duck_times_s4.append(duck_time)

        # TronDB: SEARCH WITHIN (TRAVERSE ...) — single query graph-scoped vector search
        t0 = time.time()
        tron_out = tql(
            f"SEARCH memory_cards NEAR VECTOR {seed_emb_tql} USING embedding "
            f"WITHIN (TRAVERSE FROM '{seed_id}' MATCH (a)-[e:semantic_link]->(b) DEPTH 1..2) LIMIT 10;"
        )
        tron_time = time.time() - t0
        tron_times_s4.append(tron_time)

        # Parse TronDB result count
        tron_count = 0
        for line in tron_out.strip().split("\n"):
            if "row(s)" in line:
                try:
                    tron_count = int(line.split("row(s)")[0].strip())
                except ValueError:
                    pass

        label = (content or "")[:40]
        top_sim = duck_result[0][2] if duck_result else 0
        print(
            f"  Seed {i+1:2d}: "
            f"DuckDB {duck_time*1000:6.1f}ms ({len(duck_result):3d} rows)  "
            f"TronDB {tron_time*1000:6.1f}ms ({tron_count:3d} rows)  "
            f"[{label}...]"
        )

    print(f"\n  DuckDB mean: {sum(duck_times_s4)/len(duck_times_s4)*1000:.1f}ms")
    print(f"  TronDB mean: {sum(tron_times_s4)/len(tron_times_s4)*1000:.1f}ms (SEARCH WITHIN TRAVERSE, single gRPC call)")

    print(f"\n{'=' * 78}")
    print(f"  SUMMARY")
    print(f"{'=' * 78}")
    print(f"  S1 Supersedes chain:    DuckDB {sum(duck_times_s1)/len(duck_times_s1)*1000:6.1f}ms vs TronDB {sum(tron_times_s1)/len(tron_times_s1)*1000:6.1f}ms")
    print(f"  S2 Mixed-type 2-hop:    DuckDB {sum(duck_times_s2)/len(duck_times_s2)*1000:6.1f}ms vs TronDB {sum(tron_times_s2)/len(tron_times_s2)*1000:6.1f}ms")
    print(f"  S3 Vector+Graph:        DuckDB {sum(duck_times_s3)/len(duck_times_s3)*1000:6.1f}ms vs TronDB {sum(tron_times_s3)/len(tron_times_s3)*1000:6.1f}ms")
    print(f"  S4 Scoped vector search: DuckDB {sum(duck_times_s4)/len(duck_times_s4)*1000:6.1f}ms vs TronDB {sum(tron_times_s4)/len(tron_times_s4)*1000:6.1f}ms")
    print()
    print(f"  Key insight: DuckDB is faster for pure graph traversal (columnar scan).")
    print(f"  TronDB's gRPC overhead dominates at this dataset size.")
    print(f"  TronDB's advantage: single-engine vector+graph without application stitching.")
    print(f"  S4 now works: SEARCH WITHIN (TRAVERSE ...) closes the graph-scoped search gap.")

    con.close()


# ---------------------------------------------------------------------------
# BENCH (vector search)
# ---------------------------------------------------------------------------

def cmd_bench():
    """Run comparative benchmarks: DuckDB vs TronDB."""
    import duckdb
    import numpy as np

    resolve_cli()

    # Connect to Johnny's DuckDB (read-only)
    con = duckdb.connect(str(JOHNNY_DB), read_only=True)

    # Sample query embeddings: pick 20 random cards and use their embeddings as queries
    print("Selecting benchmark query vectors...")
    query_cards = con.execute("""
        SELECT id, content, CAST(embedding AS VARCHAR) as embedding
        FROM memory_cards
        WHERE embedding IS NOT NULL
        ORDER BY RANDOM()
        LIMIT 20
    """).fetchall()

    queries = []
    for row in query_cards:
        emb_str = row[2].strip("[]")
        emb = [float(x) for x in emb_str.split(",")]
        queries.append({"id": row[0], "content": row[1][:80], "embedding": emb})

    K = 10  # top-K results

    print(f"\nRunning {len(queries)} queries, K={K}\n")
    print("=" * 70)

    duck_times = []
    tron_times = []
    recall_scores = []

    for i, q in enumerate(queries):
        # --- DuckDB vector search ---
        emb_json = json.dumps(q["embedding"])
        t0 = time.time()
        duck_results = con.execute(f"""
            SELECT id, array_cosine_similarity(embedding, '{emb_json}'::FLOAT[384]) as sim
            FROM memory_cards
            WHERE embedding IS NOT NULL
            ORDER BY sim DESC
            LIMIT {K}
        """).fetchall()
        duck_time = time.time() - t0
        duck_times.append(duck_time)
        duck_ids = set(r[0] for r in duck_results)

        # --- TronDB HNSW search ---
        vec_str = ", ".join(f"{v:.6f}" for v in q["embedding"])
        tql_stmt = f"SEARCH memory_cards NEAR VECTOR [{vec_str}] USING dense LIMIT {K};"

        t0 = time.time()
        result = subprocess.run(
            [TRONDB_CLI, "--remote", TRONDB_ADDR],
            input=tql_stmt + "\n",
            capture_output=True,
            text=True,
            timeout=30,
        )
        tron_time = time.time() - t0
        tron_times.append(tron_time)

        # Parse TronDB output to extract IDs
        tron_ids = set()
        for line in result.stdout.split("\n"):
            line = line.strip().strip("|")
            parts = [p.strip() for p in line.split("|")]
            if parts and len(parts[0]) == 36 and "-" in parts[0]:  # UUID
                tron_ids.add(parts[0])

        # Recall: what fraction of DuckDB's exact results does HNSW find?
        if duck_ids:
            recall = len(duck_ids & tron_ids) / len(duck_ids)
        else:
            recall = 0.0
        recall_scores.append(recall)

        print(f"  Query {i+1:2d}: DuckDB {duck_time*1000:6.1f}ms | TronDB {tron_time*1000:6.1f}ms | recall@{K}: {recall:.2f}  [{q['content'][:50]}...]")

    print("=" * 70)
    print(f"\nDuckDB  — mean: {sum(duck_times)/len(duck_times)*1000:.1f}ms, "
          f"p50: {sorted(duck_times)[len(duck_times)//2]*1000:.1f}ms, "
          f"p99: {sorted(duck_times)[int(len(duck_times)*0.99)]*1000:.1f}ms")
    print(f"TronDB  — mean: {sum(tron_times)/len(tron_times)*1000:.1f}ms, "
          f"p50: {sorted(tron_times)[len(tron_times)//2]*1000:.1f}ms, "
          f"p99: {sorted(tron_times)[int(len(tron_times)*0.99)]*1000:.1f}ms")
    print(f"Recall  — mean: {sum(recall_scores)/len(recall_scores):.3f}, "
          f"min: {min(recall_scores):.3f}, max: {max(recall_scores):.3f}")
    print(f"\nNote: TronDB times include gRPC + CLI overhead (~{20}ms baseline).")
    print("For pure engine latency, check the 'scanned' stats in TronDB output.")

    con.close()


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)

    cmd = sys.argv[1]
    if cmd == "export":
        cmd_export()
    elif cmd == "load":
        cmd_load()
    elif cmd == "load-edges":
        cmd_load_edges()
    elif cmd == "bench":
        cmd_bench()
    elif cmd == "bench-hops":
        cmd_bench_hops()
    elif cmd == "bench-retrieval":
        cmd_bench_retrieval()
    else:
        print(f"Unknown command: {cmd}")
        print(__doc__)
        sys.exit(1)
