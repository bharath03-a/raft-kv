#!/usr/bin/env bash
# bench.sh — Raft KV benchmark runner
#
# Starts a 3-node cluster (or 1-node for comparison), runs a suite of
# scenarios with different concurrency levels and workloads, then prints
# a consolidated report.
#
# Usage:
#   ./bench.sh                 # run full suite (durable + no-sync)
#   ./bench.sh --quick         # 5s runs instead of 15s
#   ./bench.sh --no-sync-only  # only run no-sync (fast) scenarios
#   ./bench.sh --sync-only     # only run sync (durable) scenarios

set -euo pipefail

# ── Configuration ────────────────────────────────────────────────────────────

DURATION=15        # seconds per scenario
QUICK_DURATION=5
BINARY="./target/release/raft-server"
BENCH="./target/release/raft-bench"
DATA_DIR="/tmp/raft-bench-data"
CLUSTER="127.0.0.1:7001,127.0.0.1:7002,127.0.0.1:7003"
CLUSTER_SINGLE="127.0.0.1:7001"
PIDS=()

# ── Parse flags ──────────────────────────────────────────────────────────────

QUICK=false
NO_SYNC_ONLY=false
SYNC_ONLY=false

for arg in "$@"; do
  case "$arg" in
    --quick)         QUICK=true ;;
    --no-sync-only)  NO_SYNC_ONLY=true ;;
    --sync-only)     SYNC_ONLY=true ;;
    *) echo "Unknown flag: $arg"; exit 1 ;;
  esac
done

if $QUICK; then
  DURATION=$QUICK_DURATION
fi

# ── Helpers ──────────────────────────────────────────────────────────────────

RED='\033[0;31m'; YELLOW='\033[1;33m'; GREEN='\033[0;32m'; CYAN='\033[0;36m'; NC='\033[0m'

info()    { echo -e "${CYAN}▶ $*${NC}"; }
success() { echo -e "${GREEN}✓ $*${NC}"; }
warn()    { echo -e "${YELLOW}⚠ $*${NC}"; }

cleanup() {
  info "Stopping cluster…"
  for pid in "${PIDS[@]+"${PIDS[@]}"}"; do
    kill "$pid" 2>/dev/null || true
  done
  if [[ ${#PIDS[@]} -gt 0 ]]; then
    wait "${PIDS[@]}" 2>/dev/null || true
  fi
  rm -rf "$DATA_DIR"
  info "Done."
}
trap cleanup EXIT INT TERM

require_binary() {
  if [[ ! -x "$1" ]]; then
    echo -e "${RED}Binary not found: $1${NC}"
    echo "Run:  cargo build --release --bin raft-server --bin raft-bench"
    exit 1
  fi
}

wait_for_port() {
  local host="$1" port="$2" retries=30
  while ! nc -z "$host" "$port" 2>/dev/null; do
    retries=$((retries - 1))
    if [[ $retries -eq 0 ]]; then
      echo -e "${RED}Timed out waiting for $host:$port${NC}"
      exit 1
    fi
    sleep 0.1
  done
}

# ── Cluster management ───────────────────────────────────────────────────────

start_cluster() {
  local sync_flag="${1:-}"   # empty = durable, "--no-sync" = benchmarking mode
  local label="${2:-durable}"

  info "Starting 3-node cluster ($label)…"
  rm -rf "$DATA_DIR" && mkdir -p "$DATA_DIR"
  PIDS=()

  $BINARY --id 1 --addr 127.0.0.1:7001 \
    --peers "2=127.0.0.1:7002,3=127.0.0.1:7003" \
    --data-dir "$DATA_DIR/node1" --metrics-addr 127.0.0.1:9001 $sync_flag \
    > "$DATA_DIR/node1.log" 2>&1 &
  PIDS+=($!)

  $BINARY --id 2 --addr 127.0.0.1:7002 \
    --peers "1=127.0.0.1:7001,3=127.0.0.1:7003" \
    --data-dir "$DATA_DIR/node2" --metrics-addr 127.0.0.1:9002 $sync_flag \
    > "$DATA_DIR/node2.log" 2>&1 &
  PIDS+=($!)

  $BINARY --id 3 --addr 127.0.0.1:7003 \
    --peers "1=127.0.0.1:7001,2=127.0.0.1:7002" \
    --data-dir "$DATA_DIR/node3" --metrics-addr 127.0.0.1:9003 $sync_flag \
    > "$DATA_DIR/node3.log" 2>&1 &
  PIDS+=($!)

  # Wait for all 3 to be reachable
  wait_for_port 127.0.0.1 7001
  wait_for_port 127.0.0.1 7002
  wait_for_port 127.0.0.1 7003

  # Wait for election
  info "Waiting for leader election (≈300ms)…"
  sleep 0.5
  success "Cluster ready (PIDs: ${PIDS[*]})"
}

stop_cluster() {
  for pid in "${PIDS[@]+"${PIDS[@]}"}"; do
    kill "$pid" 2>/dev/null || true
  done
  if [[ ${#PIDS[@]} -gt 0 ]]; then
    wait "${PIDS[@]}" 2>/dev/null || true
  fi
  PIDS=()
  sleep 0.3
}

# ── Benchmark runner ─────────────────────────────────────────────────────────

run_scenario() {
  local label="$1"
  local concurrency="$2"
  local workload="$3"
  local cluster="${4:-$CLUSTER}"

  printf "  %-35s " "$label"
  local out
  out=$($BENCH --cluster "$cluster" \
               --concurrency "$concurrency" \
               --duration "$DURATION" \
               --workload "$workload" 2>/dev/null)

  local ops throughput p50 p99
  ops=$(echo "$out"       | grep "Total ops"  | awk '{print $3}')
  throughput=$(echo "$out" | grep "Throughput" | awk '{print $2, $3}')
  p50=$(echo "$out"       | grep "p50"        | awk '{print $2, $3}')
  p99=$(echo "$out"       | grep "p99"        | awk '{print $2, $3}')

  printf "${GREEN}%8s ops${NC}  %12s  p50 %-12s  p99 %s\n" \
    "$ops" "$throughput" "$p50" "$p99"
}

# ── Main ─────────────────────────────────────────────────────────────────────

require_binary "$BINARY"
require_binary "$BENCH"

echo
echo "╔══════════════════════════════════════════════════════════════════════╗"
echo "║              Raft KV — Benchmark Suite                              ║"
echo "╚══════════════════════════════════════════════════════════════════════╝"
echo
echo "  Binary   $BINARY"
echo "  Bench    $BENCH"
echo "  Duration ${DURATION}s per scenario"
echo

# ── Section 1: Durable mode (fsync on every write) ───────────────────────────

if ! $NO_SYNC_ONLY; then
  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  echo "  Mode: DURABLE  (fsync on every Raft log append — full §5.4.1 safety)"
  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  echo
  start_cluster "" "durable"
  echo
  echo "  Scenario                              Ops       Throughput    Latency p50      Latency p99"
  echo "  ─────────────────────────────────────────────────────────────────────────────────────────"
  run_scenario "Write  c=1   (sequential)"     1  write
  run_scenario "Write  c=5"                    5  write
  run_scenario "Write  c=20"                  20  write
  run_scenario "Write  c=50"                  50  write
  run_scenario "Read   c=20  (read-index)"    20  read
  run_scenario "Mixed  c=20  (50/50 R+W)"     20  mixed
  echo
  stop_cluster
fi

# ── Section 2: No-sync mode (max throughput, not crash-safe) ─────────────────

if ! $SYNC_ONLY; then
  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  echo "  Mode: NO-SYNC  (fsync disabled — higher throughput, NOT crash-safe)"
  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  echo
  start_cluster "--no-sync" "no-sync"
  echo
  echo "  Scenario                              Ops       Throughput    Latency p50      Latency p99"
  echo "  ─────────────────────────────────────────────────────────────────────────────────────────"
  run_scenario "Write  c=1   (sequential)"     1  write
  run_scenario "Write  c=5"                    5  write
  run_scenario "Write  c=20"                  20  write
  run_scenario "Write  c=50"                  50  write
  run_scenario "Write  c=200 (max pressure)"  200 write
  run_scenario "Read   c=20  (read-index)"    20  read
  run_scenario "Mixed  c=20  (50/50 R+W)"     20  mixed
  echo
  stop_cluster
fi

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo
warn "All ops committed to Raft quorum (majority of nodes acknowledged)."
warn "Durable mode: every node fsyncs before replying — survives node crashes."
warn "No-sync mode: data in OS page cache only — may be lost on power failure."
echo
