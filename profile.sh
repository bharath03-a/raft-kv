#!/usr/bin/env bash
# profile.sh — generate a flamegraph of raft-server under load.
#
# Starts a 3-node cluster with nodes 2 and 3 running normally and node 1
# under a profiler. Drives load with raft-bench, then stops everything and
# opens the flamegraph.
#
# Usage:
#   ./profile.sh                          # auto-detect profiler
#   ./profile.sh --profiler=flamegraph    # force cargo-flamegraph (perf/dtrace)
#   ./profile.sh --profiler=samply        # force samply (macOS Instruments)
#   ./profile.sh --duration=30            # workload seconds (default 20)
#   ./profile.sh --concurrency=50         # bench clients (default 20)
#   ./profile.sh --no-sync                # skip fsync to profile pure consensus
#
# Prerequisites (install one):
#   cargo install flamegraph
#   cargo install samply
#
# macOS: flamegraph uses DTrace. Grant terminal.app DTrace permissions in
# System Settings > Privacy > Developer Tools if you see "dtrace: error".
# samply uses the macOS Instruments API and works without special permissions.

set -euo pipefail

# ── Defaults ─────────────────────────────────────────────────────────────────

PROFILER="auto"
DURATION=20
CONCURRENCY=20
NO_SYNC=""
DATA_DIR="/tmp/raft-profile-data"
CLUSTER="127.0.0.1:7001,127.0.0.1:7002,127.0.0.1:7003"
PIDS=()

# ── Parse flags ───────────────────────────────────────────────────────────────

for arg in "$@"; do
  case "$arg" in
    --profiler=*)   PROFILER="${arg#*=}" ;;
    --duration=*)   DURATION="${arg#*=}" ;;
    --concurrency=*) CONCURRENCY="${arg#*=}" ;;
    --no-sync)      NO_SYNC="--no-sync" ;;
    *) echo "Unknown flag: $arg"; exit 1 ;;
  esac
done

# ── Helpers ───────────────────────────────────────────────────────────────────

CYAN='\033[0;36m'; GREEN='\033[0;32m'; RED='\033[0;31m'; NC='\033[0m'
info()    { echo -e "${CYAN}> $*${NC}"; }
success() { echo -e "${GREEN}ok: $*${NC}"; }
die()     { echo -e "${RED}error: $*${NC}"; exit 1; }

cleanup() {
  info "Stopping cluster..."
  for pid in "${PIDS[@]+"${PIDS[@]}"}"; do
    kill "$pid" 2>/dev/null || true
  done
  if [[ ${#PIDS[@]} -gt 0 ]]; then
    wait "${PIDS[@]}" 2>/dev/null || true
  fi
  rm -rf "$DATA_DIR"
}
trap cleanup EXIT INT TERM

wait_for_port() {
  local host="$1" port="$2" retries=40
  while ! nc -z "$host" "$port" 2>/dev/null; do
    retries=$((retries - 1))
    [[ $retries -eq 0 ]] && die "timed out waiting for $host:$port"
    sleep 0.1
  done
}

# ── Auto-detect profiler ──────────────────────────────────────────────────────

if [[ "$PROFILER" == "auto" ]]; then
  if command -v samply &>/dev/null; then
    PROFILER="samply"
  elif command -v cargo-flamegraph &>/dev/null || cargo flamegraph --version &>/dev/null 2>&1; then
    PROFILER="flamegraph"
  else
    die "No profiler found. Install one with:\n  cargo install flamegraph\n  cargo install samply"
  fi
fi

info "Profiler: $PROFILER"
info "Duration: ${DURATION}s workload"
info "Concurrency: $CONCURRENCY clients"
[[ -n "$NO_SYNC" ]] && info "Mode: no-sync (fsync disabled)"

# ── Build with debug symbols in release mode ──────────────────────────────────

info "Building release binaries with debug symbols..."
CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release \
  --bin raft-server --bin raft-bench 2>&1 | tail -3
success "Build complete"

# ── Start background nodes (2 and 3) ─────────────────────────────────────────

rm -rf "$DATA_DIR" && mkdir -p "$DATA_DIR/node"{1,2,3}

info "Starting background nodes (2 and 3)..."

./target/release/raft-server --id 2 --addr 127.0.0.1:7002 \
  --peers "1=127.0.0.1:7001,3=127.0.0.1:7003" \
  --data-dir "$DATA_DIR/node2" $NO_SYNC \
  > "$DATA_DIR/node2.log" 2>&1 &
PIDS+=($!)

./target/release/raft-server --id 3 --addr 127.0.0.1:7003 \
  --peers "1=127.0.0.1:7001,2=127.0.0.1:7002" \
  --data-dir "$DATA_DIR/node3" $NO_SYNC \
  > "$DATA_DIR/node3.log" 2>&1 &
PIDS+=($!)

# ── Start node 1 under the profiler ──────────────────────────────────────────

info "Starting node 1 under $PROFILER..."

if [[ "$PROFILER" == "samply" ]]; then
  samply record ./target/release/raft-server \
    --id 1 --addr 127.0.0.1:7001 \
    --peers "2=127.0.0.1:7002,3=127.0.0.1:7003" \
    --data-dir "$DATA_DIR/node1" $NO_SYNC \
    > "$DATA_DIR/node1.log" 2>&1 &
  PROFILER_PID=$!
  PIDS+=($PROFILER_PID)
elif [[ "$PROFILER" == "flamegraph" ]]; then
  cargo flamegraph --no-inline --bin raft-server -- \
    --id 1 --addr 127.0.0.1:7001 \
    --peers "2=127.0.0.1:7002,3=127.0.0.1:7003" \
    --data-dir "$DATA_DIR/node1" $NO_SYNC \
    > "$DATA_DIR/node1.log" 2>&1 &
  PROFILER_PID=$!
  PIDS+=($PROFILER_PID)
else
  die "Unknown profiler: $PROFILER (use 'flamegraph' or 'samply')"
fi

# Wait for all three nodes to be reachable.
wait_for_port 127.0.0.1 7001
wait_for_port 127.0.0.1 7002
wait_for_port 127.0.0.1 7003
sleep 0.5 # allow election to complete

success "Cluster ready"

# ── Drive workload with raft-bench ────────────────────────────────────────────

info "Running ${DURATION}s write workload (c=${CONCURRENCY})..."
./target/release/raft-bench \
  --cluster "$CLUSTER" \
  --concurrency "$CONCURRENCY" \
  --duration "$DURATION" \
  --workload write

success "Workload complete"

# ── Stop the profiler (triggers report generation) ───────────────────────────

info "Stopping profiler (generating report)..."
kill -INT "$PROFILER_PID" 2>/dev/null || true
wait "$PROFILER_PID" 2>/dev/null || true

# Remove from PIDS so cleanup does not try to kill it again.
PIDS=("${PIDS[@]/$PROFILER_PID/}")

# ── Report location ───────────────────────────────────────────────────────────

echo
if [[ "$PROFILER" == "flamegraph" ]]; then
  if [[ -f flamegraph.svg ]]; then
    success "Flamegraph written to: $(pwd)/flamegraph.svg"
    # Open in browser on macOS; skip silently on Linux.
    open flamegraph.svg 2>/dev/null || true
  else
    info "flamegraph.svg not found — check $DATA_DIR/node1.log for errors"
  fi
elif [[ "$PROFILER" == "samply" ]]; then
  success "samply will open the Firefox Profiler in your browser automatically"
fi
