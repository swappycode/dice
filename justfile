set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]
set dotenv-load := true

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

# ---------- benchmark (100k-connection load gen) ----------

# Local loadgen smoke against a running gateway (Windows dev box). Start the
# gateway first (`just dev` or `just run-full`). NOTE: Windows has no quinn UDP
# GSO, so this validates CORRECTNESS (connections reach Ready + heartbeat), NOT
# throughput. The real 100k run is on Linux — see benchmarks/README.md and
# benchmarks/loadgen/bench.sh. Positional args (just is positional): conns,
# transport (quic|wss), hold seconds — e.g. `just bench 1000 quic 30`.
bench conns="1000" transport="quic" hold="30":
    $env:DICE_LOADGEN_CONNS = "{{conns}}"; $env:DICE_LOADGEN_TRANSPORT = "{{transport}}"; $env:DICE_LOADGEN_HOLD_SECS = "{{hold}}"; $env:DICE_LOADGEN_CA = "$PWD/dev/certs/dev-ca.pem"; cargo run -p dice-loadgen

# ---------- run ----------

# Fast iteration: monolith with in-proc bus + memory cache; docker Postgres only.
dev:
    $env:DICE_PROFILE = "dev-lite"; cargo run -p dice-monolith

# Monolith against the full docker infra (Postgres + Redis + NATS).
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
