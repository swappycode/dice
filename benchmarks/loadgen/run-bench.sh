#!/usr/bin/env bash
# One-shot 100k-connection benchmark for Dice (M4 scaling). Hand this to anyone
# with a fresh Ubuntu / WSL2 / cloud-Linux box: it bootstraps Rust if missing,
# brings up Postgres via Docker, builds release, runs the tuned gateway + the
# loadgen together, samples the gateway's live connection count + RSS, and prints
# a report with the numbers to send back.
#
#   git clone <repo> && cd <repo>
#   bash benchmarks/loadgen/run-bench.sh [CONNS] [RATE] [HOLD_SECS]
#   e.g.  bash benchmarks/loadgen/run-bench.sh 50000 2000 90
#
# QUIC is memory-bound (gateway + loadgen + Postgres for 100k need ~6-8 GB). On a
# small box (e.g. 8 GB) start at 30000-50000 — the per-connection RSS this prints
# extrapolates linearly to 100k. Tune the gateway by exporting DICE_QUIC_* first
# (see .env.example "QUIC server tuning" / benchmarks/README.md).
set -euo pipefail

CONNS="${1:-50000}"
RATE="${2:-500}"   # conns/sec ramp. Keep modest — too fast and the gateway can't
                   # accept in time and connections time out at connect (raise once
                   # you see a stable plateau with connect-fail≈0).
HOLD="${3:-90}"

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
echo "==> repo $(pwd) | target ${CONNS} conns @ ${RATE}/s, hold ${HOLD}s"

# --- Rust toolchain ---
if ! command -v cargo >/dev/null 2>&1; then
  echo "==> installing Rust (rustup, non-interactive)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  . "$HOME/.cargo/env"
fi

# --- Postgres (Docker) ---
if ! docker info >/dev/null 2>&1; then
  echo "ERROR: Docker daemon not reachable."
  echo "  WSL: enable Docker Desktop -> Settings -> Resources -> WSL Integration."
  echo "  Or install Postgres natively and export DATABASE_URL, then re-run."
  exit 1
fi
[ -f .env ] || cp .env.example .env
echo "==> starting Postgres"
docker compose -f infrastructure/docker/docker-compose.yml up -d --wait

# Load .env (DATABASE_URL etc.) for the raw binaries — a plain run doesn't auto-load it.
set -a; . ./.env; set +a

ulimit -n 1048576 2>/dev/null || true  # WSS needs the fds; harmless for QUIC

# Raise the UDP socket-buffer ceiling so the gateway's SO_RCVBUF/SO_SNDBUF request
# isn't silently clamped (the #1 cause of handshake timeouts at scale). Best-effort:
# needs root, skip cleanly otherwise.
if sudo -n true 2>/dev/null; then
  sudo sysctl -qw net.core.rmem_max=67108864 net.core.wmem_max=67108864 net.core.netdev_max_backlog=16384 || true
  echo "==> raised net.core.rmem_max/wmem_max to 64 MiB"
else
  echo "==> NOTE: run this once as root so SO_RCVBUF isn't clamped (else expect connect timeouts):"
  echo "         sudo sysctl -w net.core.rmem_max=67108864 net.core.wmem_max=67108864"
fi

# --- build ---
echo "==> building release (first run takes a few minutes)"
cargo build --release -p dice-monolith -p dice-loadgen

# --- gateway (tuned; every knob overridable via env) ---
GWLOG="$(mktemp)"
echo "==> starting gateway (log: $GWLOG)"
DICE_PROFILE=dev-lite DICE_ADMIN_ADDR=0.0.0.0:9600 \
DICE_QUIC_RECV_WINDOW="${DICE_QUIC_RECV_WINDOW:-262144}" \
DICE_QUIC_DATAGRAMS="${DICE_QUIC_DATAGRAMS:-false}" \
DICE_QUIC_SO_RCVBUF="${DICE_QUIC_SO_RCVBUF:-33554432}" \
DICE_QUIC_SO_SNDBUF="${DICE_QUIC_SO_SNDBUF:-33554432}" \
  ./target/release/dice-monolith > "$GWLOG" 2>&1 &
GW=$!
trap 'kill "$GW" 2>/dev/null || true' EXIT

for _ in $(seq 1 60); do
  grep -q 'bound_quic' "$GWLOG" 2>/dev/null && break
  kill -0 "$GW" 2>/dev/null || { echo "ERROR: gateway exited early:"; tail -20 "$GWLOG"; exit 1; }
  sleep 1
done
grep -q 'bound_quic' "$GWLOG" 2>/dev/null || { echo "ERROR: gateway never bound:"; tail -20 "$GWLOG"; exit 1; }
echo "==> gateway up on :8444"

# --- sampler: peak live connections + peak gateway RSS ---
SAMP="$(mktemp)"
( set +e
  while kill -0 "$GW" 2>/dev/null; do
    c=$(curl -s localhost:9600/metrics 2>/dev/null \
        | awk '/^dice_gateway_connections\{transport="quic"\}/{print int($2)}')
    r=$(awk '/^VmRSS:/{print $2}' "/proc/$GW/status" 2>/dev/null)
    [ -n "${c:-}" ] && [ -n "${r:-}" ] && echo "$c $r" >> "$SAMP"
    sleep 3
  done ) &
SAMPPID=$!

# --- loadgen ---
echo "==> running loadgen"
DICE_LOADGEN_CONNS="$CONNS" DICE_LOADGEN_RATE="$RATE" DICE_LOADGEN_HOLD_SECS="$HOLD" \
DICE_LOADGEN_REPORT_SECS=5 DICE_LOADGEN_CA=dev/certs/dev-ca.pem \
  ./target/release/dice-loadgen 2>&1 | tee /tmp/dice-loadgen-out.txt
kill "$SAMPPID" 2>/dev/null || true

# --- report ---
peak_c=$(awk '{if($1>m)m=$1}END{print m+0}' "$SAMP" 2>/dev/null || echo 0)
peak_r=$(awk '{if($2>m)m=$2}END{print m+0}' "$SAMP" 2>/dev/null || echo 0)  # KB
echo
echo "================= BENCHMARK REPORT ================="
echo "peak gateway connections (quic): ${peak_c}"
if [ "${peak_c:-0}" -gt 0 ]; then
  echo "peak gateway RSS: $((peak_r/1024)) MB   ($((peak_r/peak_c)) KB/conn)"
else
  echo "peak gateway RSS: $((peak_r/1024)) MB"
fi
echo "--- gateway UDP socket buffers (a 'clamped' line = raise net.core.rmem_max) ---"
grep -i 'UDP socket buffer' "$GWLOG" 2>/dev/null || echo "(no socket-buffer log)"
echo "--- gateway closes by code (4012=hb-timeout 4010=slow-consumer) ---"
curl -s localhost:9600/metrics 2>/dev/null | grep '^dice_gateway_closes_total' || echo "(none — good)"
echo "--- loadgen final line ---"
grep 'FINAL' /tmp/dice-loadgen-out.txt 2>/dev/null || tail -1 /tmp/dice-loadgen-out.txt
echo "==================================================="
echo "Send those 4 things back: peak conns, RSS (+KB/conn), closes, loadgen FINAL."
echo "(gateway log kept at $GWLOG; Postgres still up — 'docker compose -f infrastructure/docker/docker-compose.yml down' to stop)"
