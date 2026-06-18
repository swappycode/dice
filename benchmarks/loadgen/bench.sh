#!/usr/bin/env bash
# Linux runner for the Dice 100k-connection benchmark (M4 scaling).
#
# Raises the file-descriptor limit (needed for the WSS/TCP variant), prints the
# recommended kernel tuning, then runs dice-loadgen in release. Run it from the
# REPO ROOT (the default dev-CA path is relative). Override any DICE_LOADGEN_* /
# DICE_QUIC_* knob via the environment, e.g.:
#
#   DICE_LOADGEN_CONNS=100000 DICE_LOADGEN_RATE=2000 benchmarks/loadgen/bench.sh
#
# The gateway must already be running (see benchmarks/README.md). loadgen is the
# CLIENT half of the collaboration loop: it reports client-side numbers; read the
# server-side dice_gateway_* metrics + gateway RSS/CPU separately.
set -euo pipefail

# --- fd limit: the WSS variant needs >1 fd per connection (QUIC does not) ---
if ! ulimit -n 1048576 2>/dev/null; then
  echo "warn: could not raise 'ulimit -n' to 1048576 (need root, or set it in"
  echo "      /etc/security/limits.conf). The QUIC variant doesn't need it; WSS does."
fi

cat <<'NOTE'
# --- recommended kernel tuning (run once as root before a big run) ---
# QUIC is memory/CPU-bound and multiplexes all connections over ONE UDP socket
# per endpoint, so it needs BIG UDP socket buffers, not high fd/port limits:
#   sysctl -w net.core.rmem_max=33554432
#   sysctl -w net.core.wmem_max=33554432
#   sysctl -w net.core.netdev_max_backlog=16384
# (then set DICE_QUIC_SO_RCVBUF / DICE_QUIC_SO_SNDBUF on the GATEWAY to match.)
#
# The WSS/TCP variant instead needs file descriptors + ephemeral ports:
#   sysctl -w net.core.somaxconn=65535
#   sysctl -w net.ipv4.ip_local_port_range="1024 65535"
#   ulimit -n 1048576   (both gateway and loadgen processes)
NOTE
echo

export DICE_LOADGEN_CONNS="${DICE_LOADGEN_CONNS:-100000}"
export DICE_LOADGEN_RATE="${DICE_LOADGEN_RATE:-2000}"
export DICE_LOADGEN_HOLD_SECS="${DICE_LOADGEN_HOLD_SECS:-120}"
export DICE_LOADGEN_TRANSPORT="${DICE_LOADGEN_TRANSPORT:-quic}"
export DICE_LOADGEN_REPORT_SECS="${DICE_LOADGEN_REPORT_SECS:-5}"
export DICE_LOADGEN_TARGET="${DICE_LOADGEN_TARGET:-127.0.0.1:8444}"
export DICE_LOADGEN_WSS_URL="${DICE_LOADGEN_WSS_URL:-wss://127.0.0.1:8443/gateway/v1}"
export DICE_LOADGEN_CA="${DICE_LOADGEN_CA:-dev/certs/dev-ca.pem}"

echo "loadgen: transport=$DICE_LOADGEN_TRANSPORT conns=$DICE_LOADGEN_CONNS rate=$DICE_LOADGEN_RATE/s hold=${DICE_LOADGEN_HOLD_SECS}s"
exec cargo run --release -p dice-loadgen
