set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]
set dotenv-load := true

default:
    @just --list

# ---------- infrastructure ----------

infra-up:
    docker compose -f infrastructure/docker/docker-compose.yml up -d --wait

infra-down:
    docker compose -f infrastructure/docker/docker-compose.yml down

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

# ---------- run ----------

# Fast iteration: monolith with in-proc bus + memory cache; docker Postgres only.
dev:
    $env:DICE_PROFILE = "dev-lite"; cargo run -p dice-monolith

# Monolith against the full docker infra (Postgres + Redis + NATS).
run-full:
    $env:DICE_PROFILE = "full"; cargo run -p dice-monolith

# Desktop client dev loop (own workspace). One instance; HMR; predev frees :1420.
client:
    $env:DICE_DEV_CA = "$PWD/dev/certs/dev-ca.pem"; $env:DICE_API_URL = "https://localhost:8443"; $env:DICE_GATEWAY_QUIC = "localhost:8444"; $env:DICE_GATEWAY_WSS = "wss://localhost:8443/gateway/v1"; cd apps/desktop-client; npm run tauri dev

# Build the desktop client once (release exe, embedded UI) for the two-user demo.
client-build:
    cd apps/desktop-client; npm run build
    cd apps/desktop-client/src-tauri; cargo build --release

# Launch a built client under an ISOLATED profile (own cache + keyring + window),
# for local two-user testing. Run `just client-build` first, then e.g.
# `just client-as alice` and (second terminal) `just client-as bob`.
client-as name:
    $env:DICE_DEV_CA = "$PWD/dev/certs/dev-ca.pem"; $env:DICE_API_URL = "https://localhost:8443"; $env:DICE_GATEWAY_QUIC = "localhost:8444"; $env:DICE_GATEWAY_WSS = "wss://localhost:8443/gateway/v1"; & "apps/desktop-client/src-tauri/target/release/dice-desktop.exe" --profile {{name}}
