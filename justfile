set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]
set dotenv-load := true
# Lets the same recipe name have [windows] + [unix] variants (the OS attributes
# pick which one is active), so the run/bench recipes work on Windows AND in WSL.
set allow-duplicate-recipes := true

default:
    @just --list

# ---------- infrastructure ----------

infra-up:
    docker compose -f infrastructure/docker/docker-compose.yml up -d --wait

infra-down:
    docker compose -f infrastructure/docker/docker-compose.yml down

# ---------- observability (opt-in) ----------

# Prometheus + Grafana scraping the gateway/service /metrics ports. Run the app
# (`just run-full` or `just split-up`) so there are live targets, then open
# Grafana at http://localhost:3000 (anonymous admin) — the "Dice" dashboard is
# provisioned. The app's admin port must bind 0.0.0.0 (the default) so the
# dockerized Prometheus can reach it via host.docker.internal.
metrics-up:
    docker compose -f infrastructure/docker/observability.yml up -d

metrics-down:
    docker compose -f infrastructure/docker/observability.yml down

# ---------- database ----------

db-setup:
    sqlx database create --database-url $env:DATABASE_URL
    sqlx migrate run --source crates/database/migrations --database-url $env:DATABASE_URL

db-reset:
    sqlx database reset -y --source crates/database/migrations --database-url $env:DATABASE_URL

# Regenerate the committed .sqlx offline cache after query/migration changes.
sqlx-prepare: db-setup
    cargo sqlx prepare --workspace -- --all-targets

sqlx-check:
    cargo sqlx prepare --check --workspace -- --all-targets

# ---------- quality gates ----------

check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
    @just gate-aws-lc

# aws-lc-sys must never enter the tree (NASM/CMake trap on Windows; ring-only policy).
gate-aws-lc:
    $out = cargo tree -i aws-lc-sys 2>&1 | Out-String; if ($out -notmatch 'nothing to print|did not match any packages|error: package ID specification') { Write-Error "aws-lc-sys found in dependency tree!`n$out" } else { Write-Host "aws-lc-sys gate: clean" }

# Supply-chain audit (cargo-deny): advisories + permissive licenses + the
# ring-only ban (config in deny.toml). Install once: `cargo install cargo-deny`.
deny:
    cargo deny check advisories licenses bans sources

# ---------- benchmark (100k-connection load gen) ----------

# Terminal 1: the gateway tuned for the benchmark — dev-lite (Postgres only),
# RELEASE build, /metrics on 0.0.0.0:9600. Prefix DICE_QUIC_* to tune, e.g.
# (WSL/Linux)  DICE_QUIC_RECV_WINDOW=262144 DICE_QUIC_DATAGRAMS=false just bench-server
# See .env.example "QUIC server tuning" + benchmarks/README.md. Needs DATABASE_URL
# (in .env; `cp .env.example .env`) and `just infra-up` for Postgres.
[unix]
bench-server:
    DICE_PROFILE=dev-lite DICE_ADMIN_ADDR=0.0.0.0:9600 cargo run --release -p dice-monolith

[windows]
bench-server:
    $env:DICE_PROFILE = "dev-lite"; $env:DICE_ADMIN_ADDR = "0.0.0.0:9600"; cargo run --release -p dice-monolith

# Terminal 2: the load generator (run `just bench-server` first). Positional args
# (just is positional): conns, transport (quic|wss), hold seconds —
# e.g. `just bench 100000 quic 120`. On WSL/Linux this is the REAL run (release +
# UDP GSO); on the Windows host it's a debug correctness smoke only (no GSO).
[unix]
bench conns="1000" transport="quic" hold="30":
    DICE_LOADGEN_CONNS={{conns}} DICE_LOADGEN_TRANSPORT={{transport}} DICE_LOADGEN_HOLD_SECS={{hold}} DICE_LOADGEN_CA="$PWD/dev/certs/dev-ca.pem" cargo run --release -p dice-loadgen

[windows]
bench conns="1000" transport="quic" hold="30":
    $env:DICE_LOADGEN_CONNS = "{{conns}}"; $env:DICE_LOADGEN_TRANSPORT = "{{transport}}"; $env:DICE_LOADGEN_HOLD_SECS = "{{hold}}"; $env:DICE_LOADGEN_CA = "$PWD/dev/certs/dev-ca.pem"; cargo run -p dice-loadgen

# ---------- run ----------

# Fast iteration: monolith with in-proc bus + memory cache; docker Postgres only.
[unix]
dev:
    DICE_PROFILE=dev-lite cargo run -p dice-monolith

[windows]
dev:
    $env:DICE_PROFILE = "dev-lite"; cargo run -p dice-monolith

# Monolith against the full docker infra (Postgres + Redis + NATS).
[unix]
run-full:
    DICE_PROFILE=full cargo run -p dice-monolith

[windows]
run-full:
    $env:DICE_PROFILE = "full"; cargo run -p dice-monolith

# Split-mode "microservices" demo: the gateway (monolith with DICE_SPLIT=1)
# routes auth/chat/presence over NATS RPC to three standalone service bins,
# each in its own window. Same code as `run-full`, just decomposed. Run
# `just infra-up` first (needs the full infra: Postgres + Redis + NATS).
#
# The gateway boots FIRST: it generates the dev TLS/JWT assets and runs the DB
# migrations, then serves. After a short pause the three services start (they
# load the SAME JWT keys + an already-migrated DB). Each process gets a DISTINCT
# DICE_NODE_ID so snowflake ids never collide. Ctrl-C / close each window to stop.
split-up:
    cargo build -p dice-monolith -p auth-service -p chat-service -p presence-service
    Start-Process powershell -WorkingDirectory $PWD -ArgumentList '-NoExit','-Command',"`$env:DICE_PROFILE='full'; `$env:DICE_SPLIT='1'; `$env:DICE_NODE_ID='0'; `$env:DICE_ADMIN_ADDR='0.0.0.0:9600'; `$env:DICE_SERVICE_NAME='gateway'; `$env:DICE_OTLP_ENDPOINT='http://localhost:4318'; cargo run -p dice-monolith"
    Start-Sleep -Seconds 5
    Start-Process powershell -WorkingDirectory $PWD -ArgumentList '-NoExit','-Command',"`$env:DICE_NODE_ID='1'; `$env:DICE_ADMIN_ADDR='0.0.0.0:9601'; `$env:DICE_SERVICE_NAME='auth-service'; `$env:DICE_OTLP_ENDPOINT='http://localhost:4318'; cargo run -p auth-service"
    Start-Process powershell -WorkingDirectory $PWD -ArgumentList '-NoExit','-Command',"`$env:DICE_NODE_ID='2'; `$env:DICE_ADMIN_ADDR='0.0.0.0:9602'; `$env:DICE_SERVICE_NAME='chat-service'; `$env:DICE_OTLP_ENDPOINT='http://localhost:4318'; cargo run -p chat-service"
    Start-Process powershell -WorkingDirectory $PWD -ArgumentList '-NoExit','-Command',"`$env:DICE_NODE_ID='3'; `$env:DICE_ADMIN_ADDR='0.0.0.0:9603'; `$env:DICE_SERVICE_NAME='presence-service'; `$env:DICE_OTLP_ENDPOINT='http://localhost:4318'; cargo run -p presence-service"
    Write-Host "split fleet launched (4 windows): gateway + auth/chat/presence. /metrics: 9600 gateway, 9601/9602/9603 auth/chat/presence. Traces -> Tempo (run 'just metrics-up' first). Point the client at https://localhost:8443; Ctrl-C each window to stop."

# Desktop client dev loop (own workspace). One instance; HMR; predev frees :1420.
client:
    $env:DICE_DEV_CA = "$PWD/dev/certs/dev-ca.pem"; $env:DICE_API_URL = "https://localhost:8443"; $env:DICE_GATEWAY_QUIC = "localhost:8444"; $env:DICE_GATEWAY_WSS = "wss://localhost:8443/gateway/v1"; cd apps/desktop-client; npm run tauri dev

# Build the desktop client once (release exe, embedded UI) for the two-user demo.
# `--features custom-protocol` selects a PRODUCTION build so the exe serves its
# embedded UI instead of trying the dev server (localhost:1420).
client-build:
    cd apps/desktop-client; npm run build
    cd apps/desktop-client/src-tauri; cargo build --release --features custom-protocol

# Launch a built client under an ISOLATED profile (own cache + keyring + window),
# for local two-user testing. Run `just client-build` first, then e.g.
# `just client-as alice` and (second terminal) `just client-as bob`.
client-as name:
    $env:DICE_DEV_CA = "$PWD/dev/certs/dev-ca.pem"; $env:DICE_API_URL = "https://localhost:8443"; $env:DICE_GATEWAY_QUIC = "localhost:8444"; $env:DICE_GATEWAY_WSS = "wss://localhost:8443/gateway/v1"; & "apps/desktop-client/src-tauri/target/release/dice-desktop.exe" --profile {{name}}

# Measure idle RAM of the release client at the login screen (private commit,
# summed over host + WebView2 tree; compare to the <100 MB M2 goal). Run
# `just client-build` first. A/B a browser-arg experiment by setting the env
# first, e.g.: $env:DICE_WEBVIEW_ARGS = "--in-process-gpu ..."; just client-measure
client-measure idle="30":
    powershell -NoProfile -ExecutionPolicy Bypass -File apps/desktop-client/scripts/measure-ram.ps1 -Idle {{idle}}
