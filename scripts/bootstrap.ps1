# One-time cold-machine setup: toolchain + just + sqlx-cli.
# Everything else flows through `just` (run `just --list`).
$ErrorActionPreference = "Stop"

Write-Host "== Dice bootstrap ==" -ForegroundColor Cyan

# 1. Toolchain (rust-toolchain.toml auto-installs the pinned version on first cargo use).
rustup show

# 2. MSYS2 hazard check: a MinGW cmake on PATH poisons MSVC -sys crate builds.
$cmake = Get-Command cmake -ErrorAction SilentlyContinue
if ($cmake -and $cmake.Source -match 'msys64') {
    Write-Warning "MSYS2 MinGW cmake found on PATH ($($cmake.Source)). The ring-only TLS policy avoids needing cmake, but if any future dependency invokes it, builds will fail in confusing ways. Consider reordering PATH."
}

# 3. just (task runner)
if (-not (Get-Command just -ErrorAction SilentlyContinue)) {
    Write-Host "Installing just..." -ForegroundColor Cyan
    cargo install just --locked
}

# 4. sqlx-cli — pinned to the workspace sqlx 0.8.x minor; minimal features.
if (-not (Get-Command sqlx -ErrorAction SilentlyContinue)) {
    Write-Host "Installing sqlx-cli 0.8.x..." -ForegroundColor Cyan
    cargo install sqlx-cli --version "^0.8" --locked --no-default-features --features rustls,postgres
}

Write-Host "Bootstrap complete. Next: copy .env.example to .env, then 'just infra-up && just db-setup'." -ForegroundColor Green
