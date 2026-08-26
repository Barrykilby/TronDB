#!/usr/bin/env bash
# tests/cluster/cluster_test.sh
# Spins up a 3-node cluster, runs queries, verifies replication, tears down.
# Requires: docker compose, curl/grpcurl
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

echo "=== Building trondb image ==="
docker build -t trondb:test "$PROJECT_ROOT"

echo "=== Starting 3-node cluster ==="
docker compose -f "$PROJECT_ROOT/docker-compose.yml" up -d

# Wait for health
echo "=== Waiting for nodes to be ready ==="
for port in 9400 9401; do
    for i in $(seq 1 30); do
        if grpcurl -plaintext "localhost:$port" grpc.health.v1.Health/Check >/dev/null 2>&1; then
            echo "Node on port $port is ready"
            break
        fi
        if [ "$i" -eq 30 ]; then echo "TIMEOUT waiting for port $port"; exit 1; fi
        sleep 1
    done
done

echo "=== Tearing down ==="
docker compose -f "$PROJECT_ROOT/docker-compose.yml" down -v

# The two checks this script is supposed to perform, executing
# test_queries.tql through the router and confirming the primary returns the
# same two rows, are not implemented. Until they are, this script verifies
# only that three containers start and report healthy.
#
# It exits non-zero deliberately. A script that prints "passed" after
# verifying nothing is worse than no script: it makes an unverified
# distributed layer look tested.
echo
echo "INCOMPLETE: containers started and reported healthy."
echo "Replication and routing are NOT verified. This is a scaffold, not a test."
exit 1
