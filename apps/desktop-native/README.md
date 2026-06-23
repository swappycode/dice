# dice-native

The **native Rust desktop client** for Dice — a [Slint](https://slint.dev) UI built
to replace the WebView2-hosted client and break the **< 100 MB idle-RAM** goal
(WebView2 floors at ~117 MB; this lands at ~10 MB private / ~30 MB working set).

It is its own cargo workspace (excluded from the root Dice workspace, like
`apps/desktop-client/src-tauri`) so the GUI dependency tree never touches server
builds. Toolkit = Slint with the **pure-Rust software renderer** — no GPU, no C
toolchain, hermetic (no `aws-lc`/`openssl`/native-build traps).

## Status

**Milestone 1 — the UI shell on seed data — COMPLETE + polished** across all 8 themes
(login, register, app shell, chat, voice, home/friends, and the Settings / Add-Friend /
Add-a-Server / Server-Settings dialogs). The frameless window drags, resizes, has Windows 11
rounded corners + an embedded taskbar icon, and a Per-Monitor-V2 DPI manifest for crisp text.
RAM: **~10.6 MB private / ~29.7 MB working set** (vs WebView2 ≈117 MB).

The backend wiring (real login / guilds / messages / voice via the existing `ClientCore`
host) is **milestone 2** — until then the data on screen is placeholder seed data.

Rendering note: the Slint software renderer rounds **only solid-colour fills** (not gradients
or `clip`), so every accent surface uses a solid fill; top-only rounding (card / dialog
headers) is a solid base + a per-corner-radius body-cover.

## Run

```sh
cargo run --release                 # lean build
cargo run -- --start voice          # jump to a screen: login | chat | voice | home
```

The custom title bar's min / max / close work; the window is frameless.

## Headless screenshots (no display needed)

The software renderer can dump any screen in all 8 themes to PNG:

```sh
cargo run -- --shots shots chat     # login | chat | voice | home | d-theme | d-guild | d-security | d-voice
```

PNGs land in `shots/` (gitignored). Used to verify fidelity while building.

## Layout

- `ui/theme.slint` — the 8 palettes (24-token contract) + geometry tokens
- `ui/state.slint` — the `State` global + row structs + the animation clock
- `ui/widgets.slint`, `ui/icons.slint` — generated primitives (die logo, avatars, vector icons)
- `ui/login.slint`, `ui/shell.slint`, `ui/dialogs.slint` — screens + modals
- `ui/app.slint` — the entry `AppWindow`
- `src/main.rs` — entry, font registration, the `--shots` harness, window controls
- `src/seed.rs` — milestone-1 demo data (replaced by `ClientCore` in M2)
- `fonts/` — bundled OFL fonts (see `fonts/NOTICE.md`)

## RAM

```powershell
powershell -File scripts\measure-ram.ps1 [-Screen chat]
```

Single native process (no WebView2 tree) — one `Get-Process` read is the whole story.
