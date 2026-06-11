#!/usr/bin/env bash
# One-time cold-machine setup: toolchain + just + sqlx-cli. (Linux/macOS/CI mirror of bootstrap.ps1)
set -euo pipefail

echo "== Dice bootstrap =="
rustup show

command -v just >/dev/null 2>&1 || cargo install just --locked
command -v sqlx >/dev/null 2>&1 || cargo install sqlx-cli --version "^0.8" --locked --no-default-features --features rustls,postgres

echo "Bootstrap complete. Next: cp .env.example .env, then 'just infra-up && just db-setup'."
