# Benchmarks

The headline M4 (scaling) gate: **100k concurrent connections** on one gateway.
`loadgen/` is the load generator that drives it.

> **Do not benchmark QUIC throughput on Windows** — Windows lacks quinn's UDP GSO
> batching, so numbers there are meaningless. Windows is fine for *correctness*
> smokes (connections reach Ready + heartbeat); run the real gate on Linux.

## Results

Ubuntu 22.04 (WSL2), gateway + loadgen co-located, QUIC over loopback, dev-lite
(Postgres only). Gateway tuned with `DICE_QUIC_RECV_WINDOW=262144
DICE_QUIC_DATAGRAMS=false DICE_QUIC_SO_RCVBUF/SNDBUF=32 MiB` (effective 64 MiB
after `net.core.rmem_max=64 MiB`); each connection held with 30 s heartbeats.

| Connections | Ramp | Gateway RSS | KB/conn (incl. base) | connect p99 | hb-RTT p99 | connect-fail | closes |
|---|---|---|---|---|---|---|---|
| **30,000** | 500/s | **1.71 GB** | **58** | 200 ms | 100 ms | **0** | none |
| 5,537\* | 2000/s | 665 MB | 120 | 2 s | 10 s | 22,249\* | none |

\* The 2000/s run saturated the accept path *before* `net.core.rmem_max` was
raised (the 32 MiB `SO_RCVBUF` was kernel-clamped → dropped handshake packets);
kept only to bracket the memory curve.

**Per-connection memory.** The two points fit `RSS ≈ 429 MB base + ~44 KB/conn`,
so one gateway node extrapolates to **~4.7 GB at 100k connections** — well within
a commodity server. At 30k it sheds nothing (`dice_gateway_closes_total` empty)
and heartbeat RTT stays ~1 ms (p50). The only blocker to a *literal* 100k on a
single WSL2 box is RAM (the gateway **and** the load generator share it); the
per-connection cost itself is comfortable.


## What `dice-loadgen` does

Opens N concurrent **QUIC** (or **WSS**) connections to a running gateway, drives
each through the real `Hello → Identify → Ready` handshake, and holds it open with
app heartbeats — reporting, every few seconds:

- `live` — currently-established connections (the client mirror of the gateway's
  `dice_gateway_connections{transport}` gauge),
- `established` / `connect-fail` / `handshake-fail` / `disconnected` (with a
  close-code breakdown: 4012 heartbeat-timeout, 4010 slow-consumer, …),
- connect-latency and heartbeat-RTT percentiles (p50/p99).

It is the **client half** of the benchmark. Read the server-side `dice_gateway_*`
metrics and the gateway's RSS/CPU separately (see *Watch* below) — together they
answer "does one gateway sustain 100k, and at what memory/CPU?".

### How it scales to 100k (design notes)

- **Offline-minted tokens.** The gateway verifies access JWTs cryptographically at
  Identify (public key only — no DB, no auth-service hop) and accepts tokens for
  users that don't exist in Postgres. So loadgen mints its own from the gateway's
  dev Ed25519 key (`dev/keys/jwt_ed25519.pem`): **zero DB seeding, zero REST/login
  traffic.**
- **Shared QUIC endpoints.** All connections share a small pool of `quinn::Endpoint`s
  (one per core by default) instead of one UDP socket per connection — bounding
  socket/fd count and letting GSO batch sends. (QUIC multiplexes connections over
  one socket by connection id, so 100k QUIC connections use only *pool*-many source
  ports — fd/ephemeral-port limits are a WSS concern, not a QUIC one.)
- **Thin frame I/O.** Hand-rolled over the one sanctioned `dice-protocol` codec; no
  per-connection driver tasks/channels.

## Quick start (local correctness smoke)

```bash
just infra-up                 # Postgres/Redis/NATS
just dev                      # gateway, dev-lite (separate terminal)
just bench 1000 quic 30       # Windows (positional args: conns transport hold)
# or directly, any OS, from the repo root:
DICE_LOADGEN_CONNS=1000 DICE_LOADGEN_CA=dev/certs/dev-ca.pem cargo run -p dice-loadgen
```

Expect `live=1000`, `connect-fail=0`, `handshake-fail=0`, and heartbeat `ack`
keeping pace with `sent`.

## The 100k run (Linux)

loadgen and the gateway run on the **same** Linux box (the dev cert only has
`localhost`/`127.0.0.1`/`::1` SANs, so dial loopback).

**One-shot (hand it to anyone with a Linux/WSL2 box):**
```bash
git clone <repo> && cd <repo>
bash benchmarks/loadgen/run-bench.sh 50000 2000 90      # CONNS RATE HOLD
```
`run-bench.sh` bootstraps Rust, brings up Postgres (Docker), builds release, runs
the tuned gateway + loadgen together, samples peak connections + gateway RSS, and
prints a report. The manual steps below are the same thing, broken out.

**1. Boot the gateway once** (generates `dev/certs` + `dev/keys`), then with tuning:

```bash
# dev-lite (Postgres only) is enough for a connection-hold soak; full adds Redis/NATS.
DICE_PROFILE=dev-lite \
DICE_ADMIN_ADDR=0.0.0.0:9600 \            # so you can scrape /metrics
DICE_QUIC_RECV_WINDOW=262144 \            # 256 KiB/conn (down from 4 MiB) — the memory dial
DICE_QUIC_STREAM_RECV_WINDOW=131072 \
DICE_QUIC_MAX_UNI_STREAMS=0 \             # Dice opens none
DICE_QUIC_DATAGRAMS=false \               # no voice in the bench: saves ~128 KiB/conn
DICE_QUIC_SO_RCVBUF=33554432 \            # 32 MiB UDP buffers (match net.core.*mem_max)
DICE_QUIC_SO_SNDBUF=33554432 \
DICE_HEARTBEAT_MS=30000 \
cargo run --release -p dice-monolith
```

All `DICE_QUIC_*` default to the protocol production values, so start from those
and tighten as RSS demands. Full reference: `.env.example` (`QUIC server tuning`).

**2. Tune the OS** (once, as root). QUIC is memory/CPU-bound and wants big UDP
buffers; WSS instead wants fds + ephemeral ports:

```bash
# QUIC
sysctl -w net.core.rmem_max=33554432 net.core.wmem_max=33554432
sysctl -w net.core.netdev_max_backlog=16384
# WSS (TCP) also:
sysctl -w net.core.somaxconn=65535 net.ipv4.ip_local_port_range="1024 65535"
ulimit -n 1048576              # in the gateway + loadgen shells
```

**3. Run loadgen** (raises `ulimit -n`, prints the sysctls, runs release):

```bash
DICE_LOADGEN_CONNS=100000 DICE_LOADGEN_RATE=2000 benchmarks/loadgen/bench.sh
```

Ramp slowly at first (`DICE_LOADGEN_RATE=1000`) and watch for `connect-fail` /
`handshake-fail` climbing — that's where a limit binds. Increase the rate once a
plateau holds.

**4. Watch** (the numbers to report back):

```bash
curl -s localhost:9600/metrics | grep -E '^dice_gateway_(connections|closes_total|frames_total)'
# gateway process RSS/CPU:
ps -o rss=,pcpu= -p "$(pgrep -f dice-monolith)"   # RSS in KB
```

- `dice_gateway_connections{transport="quic"}` → target **~100k sustained**.
- `dice_gateway_closes_total{code}` → spikes in `4010` (slow consumer) or `4012`
  (heartbeat timeout) mean the gateway is shedding under load.
- **Gateway RSS at 100k** is the key scaling number — divide by 100k for the
  per-connection cost; shrink `DICE_QUIC_RECV_WINDOW` if it's too high.

## Running it in WSL2 (Windows)

WSL2 is the supported way to run the *real* benchmark from a Windows machine — it
has quinn's UDP GSO, the Windows host doesn't. Everything below runs **inside** WSL
(`wsl` from a terminal); gateway and loadgen share the box and dial loopback.

**One-time setup:**

```bash
# Rust + build deps
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
sudo apt update && sudo apt install -y build-essential pkg-config

# just (apt's package is too old on Ubuntu 22.04)
cargo install just
```

For Postgres, enable **Docker Desktop → Settings → Resources → WSL Integration**
for your distro (then `docker info` works inside WSL), or install native Postgres
and match `DATABASE_URL` (`...@localhost:5433/dice` — note **port 5433**).

**Clone into WSL's native filesystem**, *not* `/mnt/d` — building on the Windows
mount is slow and fights the Windows `target/` dir:

```bash
git clone <repo> ~/dice && cd ~/dice
cp .env.example .env
just infra-up        # Postgres/Redis/NATS; for only Postgres (saves RAM):
                     #   docker compose -f infrastructure/docker/docker-compose.yml up -d --wait postgres
```

**Three terminals**, all in `~/dice`. This is the `just`-recipe form of the manual
run above — knob meanings are in *The 100k run (Linux)*, step 1, and the OS buffer
sysctls from step 2 apply here too (WSL2 is a real kernel):

```bash
# Terminal 1 — gateway. `just bench-server` already sets dev-lite + admin :9600;
# prefix only the tuning. First --release build takes a few minutes.
DICE_QUIC_RECV_WINDOW=262144 DICE_QUIC_STREAM_RECV_WINDOW=131072 \
DICE_QUIC_MAX_UNI_STREAMS=0 DICE_QUIC_DATAGRAMS=false \
DICE_QUIC_SO_RCVBUF=33554432 DICE_QUIC_SO_SNDBUF=33554432 \
DICE_HEARTBEAT_MS=30000 just bench-server
#   wait for: bound_quic=0.0.0.0:8444

# Terminal 2 — loadgen (positional: conns transport hold). Trial, confirm fail=0, ramp:
just bench 5000 quic 30        # expect live ≈ 5000, fail = 0
just bench 30000 quic 60

# Terminal 3 — watch (see The 100k run, step 4 for how to read these)
watch -n2 'curl -s localhost:9600/metrics | grep -E "^dice_gateway_(connections|closes_total)"; ps -o rss=,pcpu= -p $(pgrep -f dice-monolith)'
```

> **Memory ceiling.** gateway + loadgen + Postgres are memory-bound; a ~7–8 GB WSL
> tops out around **30k–50k** connections — plenty to read per-conn RSS off
> `/metrics` and prove the curve is linear (it extrapolates to 100k). To actually
> reach 100k, give WSL more RAM via `C:\Users\<you>\.wslconfig` on the Windows side:
> ```ini
> [wsl2]
> memory=12GB
> ```
> then `wsl --shutdown` from Windows PowerShell and reopen WSL (worth it only if the
> host has ≥ 16 GB).

## `DICE_LOADGEN_*` reference

| Variable | Default | Meaning |
|---|---|---|
| `DICE_LOADGEN_TRANSPORT` | `quic` | `quic` or `wss` |
| `DICE_LOADGEN_TARGET` | `127.0.0.1:8444` | QUIC `host:port` |
| `DICE_LOADGEN_WSS_URL` | `wss://127.0.0.1:8443/gateway/v1` | WSS URL |
| `DICE_LOADGEN_CONNS` | `1000` | total connections to open |
| `DICE_LOADGEN_RATE` | `500` | connections/sec during the ramp (`0` = all at once) |
| `DICE_LOADGEN_HOLD_SECS` | `60` | hold after the ramp (`0` = until Ctrl-C) |
| `DICE_LOADGEN_ENDPOINTS` | #cores | shared QUIC client endpoints (UDP sockets) |
| `DICE_LOADGEN_HEARTBEAT_MS` | `0` | override beat cadence (`0` = use the server's `Hello` value) |
| `DICE_LOADGEN_CA` | `dev/certs/dev-ca.pem` | dev CA the client trusts |
| `DICE_LOADGEN_JWT_PRIVATE` | `dev/keys/jwt_ed25519.pem` | signing key (same one the gateway loads) |
| `DICE_LOADGEN_REPORT_SECS` | `5` | stats print interval |
| `DICE_LOADGEN_SERVER_NAME` | host of target | TLS SNI override (must match a cert SAN) |
| `DICE_LOADGEN_CAPABILITIES` | `0` | `Identify.capabilities` bits |

Gateway-side `DICE_QUIC_*` / `DICE_HEARTBEAT_MS` knobs: see `.env.example`.
