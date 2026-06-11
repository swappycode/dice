# Dice desktop client (frontend)

Vite + SolidJS + TypeScript frontend for the Dice chat platform, styled with the
**Dice Retro UI** system (Luna/XP + Aero/Win7 themes, `docs/design/retro-ui.md`).

## Run it standalone (mock mode)

```sh
npm install
npm run dev        # http://localhost:1420 (strictPort)
```

There is no backend and no Tauri host yet — the app runs against an in-memory
mock of the IPC bridge (`src/lib/ipc.mock.ts`): 2 guilds, 4 channels, 2 DMs,
~30 seeded messages, 4 other users with varied presence. Any e-mail/password
logs in. Sends echo back after 150 ms (optimistic nonce reconcile); a fake
incoming message + typing indicator fires every ~20 s while the tab is visible.
The session sticks in `localStorage` (`dice.mock.session`) — use "Log off" in
the sidebar footer to get back to the login card.

Other scripts:

```sh
npm run check      # tsc --noEmit
npm run build      # production build into dist/
```

## The IPC seam

UI code talks only to the `DiceIpc` interface in `src/lib/ipc.ts`
(commands + `onEvent()` subscription delivering typed `DiceEvent`s).
Implementation is picked by `VITE_MOCK_IPC` (default `"true"`); the mock is
also forced whenever `window.__TAURI__` is absent, so plain-browser dev always
works. All entity ids are **strings** (u64 snowflakes overflow JS numbers).

## What arrives in later phases

- `src-tauri/` — the Tauri 2 host (Rust): real window controls for the custom
  titlebar (drag regions + feature detection are already in place), keyring
  session storage, SQLite cache, and `network-core` (QUIC/WSS gateway).
- The host implements the same `DiceIpc` surface over `invoke`/`listen`
  (`dice://event` channel) — the mock is deleted-in-place, no UI changes.
- Post-M1 theme `midnight` (dark Aero), sounds (`lib/sounds.ts`), unread
  markers, virtualized message list (current shape is the pre-approved
  "last-100 + Load older" escape hatch).

## Style rules (binding, from retro-ui.md)

- Two-layer token contract in `src/styles/tokens.css`; themes override Layer B
  only (`src/themes/luna.css`, `src/themes/aero.css`).
- Component CSS modules consume `var(--*)` tokens only — **no raw hex** in
  `src/components/**` (grep for `#` to verify).
- No webfonts, no raster images, no infinite animations (typing dots mount
  only while someone types; `html.app-idle` pauses animations when the window
  is hidden/blurred; `prefers-reduced-motion` kills them).
- Built CSS budget: < 100 KB raw total.
