> Preserved milestone-1 design document (agent-produced, 2026-06-11).
> Where this conflicts with docs/protocol.md or the critiques' resolutions, those win.

# Dice Retro UI System — Design Doc

**Codename: "Comeback"** — a Luna/Aero-inspired design token system for the Dice chat client (Tauri 2 + SolidJS + Vite, Windows-first).

---

## 1. Library Evaluation & Recommendation

### Survey (verified June 2026)

| Library | License | Size (min / gz) | Assets embedded | Theming | Notes |
|---|---|---|---|---|---|
| **98.css** (jdan) | MIT | 26.6 KB / 4.4 KB | `@font-face` "Pixelated MS Sans Serif" (community-recreated font, woff/woff2 files) | CSS custom properties | Win98 aesthetic — wrong era for us; great reference for bevel technique |
| **XP.css** (botoxparty) | MIT | 256 KB / 39 KB | 40–50+ **hand-recreated SVG data URIs** (window buttons, scrollbar arrows, checkboxes); pixel fonts via `@font-face` | SCSS source; XP + 98 themes as *separate stylesheets*, "include only one at a time" — no runtime variable switching | Fork/extension of 98.css. Big dist; pixel-font dependency conflicts with our Tahoma direction |
| **7.css** (khang-nd) | MIT | 86.4 KB / 19.7 KB | Mixed: **base64 PNG bitmaps** (scrollbar thumbs, progress patterns) + SVG data URIs | **No CSS custom properties** — all colors hardcoded | System fonts only (Segoe UI) — good; PNG bitmaps have unclear provenance (possibly sampled from real Win7 chrome) |

### Why none of them is a direct dependency

1. **No dual-theme story.** We need Luna ↔ Aero switchable at runtime. XP.css and 7.css are separate packages with *different class vocabularies* and hardcoded colors; switching would mean shipping ~340 KB of combined CSS and writing two parallel component markups. That kills the < 100 KB budget and the maintainability story.
2. **They are demo kits, not app design systems.** Both style bare element selectors (`button`, `input`, `pre`) and `.window`/`.title-bar` demo conventions. In a real app this causes global-selector collisions and specificity fights with layout CSS.
3. **We need ~25% of their surface.** No tabs, progress bars, balloons, tree-view-with-checkboxes, etc. in Milestone 1.
4. **Asset provenance risk.** 7.css embeds base64 *PNG bitmaps* that look sampled from actual Win7 chrome. Even under the repo's MIT license, Microsoft's underlying artwork copyright wouldn't be cleared by that. XP.css's SVGs are hand-redrawn (safer), but pixel-faithful recreations of MS art are still gray-zone.

### Recommendation: **hand-roll a token-driven system; vendor recipes, not files**

Build **"Dice Retro UI"** in-house: a two-layer CSS custom-property token system (`geometry/elevation` layer + `color/texture` layer) with two M1 themes — **Luna** (XP vibe) and **Aero** (Win7 vibe) — switched via `data-theme` on `<html>`.

- **Vendor as reference, with attribution:** copy/adapt individual *recipes* (bevel box-shadow stacks, scrollbar selectors, gloss gradient stops) from 98.css/XP.css/7.css under MIT, with a `THIRD_PARTY_NOTICES` entry. **Do not copy any data-URI image from 7.css** (PNG provenance) — redraw all glyphs as our own inline SVGs (~10 tiny ones needed).
- Estimated total CSS: **50–65 KB unminified, ~10 KB gz** — half the budget, both themes always loaded (no theme-switch FOUC, no async CSS).

---

## 2. Theme Architecture

### 2.1 File structure

```
apps/desktop-client/
  src/
    styles/
      tokens.css          ← semantic token contract + geometry defaults (~6 KB)
      base.css            ← reset, typography, focus rings, selection (~4 KB)
      scrollbars.css      ← ::-webkit-scrollbar recipes, token-driven (~4 KB)
      recipes.css         ← shared utility classes: .bevel-raised, .bevel-sunken,
                            .gloss, .glass-panel, .orb (~5 KB)
    themes/
      luna.css            ← [data-theme="luna"] color/texture overrides (~8 KB)
      aero.css            ← [data-theme="aero"] color/texture overrides (~10 KB)
      midnight.css        ← (post-M1) dark Aero variant (~6 KB)
    components/
      chrome/TitleBar.tsx        + TitleBar.module.css
      chrome/StatusBar.tsx       + StatusBar.module.css
      auth/LoginCard.tsx         + LoginCard.module.css
      guilds/GuildRail.tsx       + GuildRail.module.css
      channels/ChannelTree.tsx   + ChannelTree.module.css
      messages/MessageList.tsx   + MessageList.module.css
      messages/Composer.tsx      + Composer.module.css
      presence/PresenceOrb.tsx   (inline SVG, no CSS file)
      presence/TypingIndicator.tsx + TypingIndicator.module.css
    lib/theme.ts          ← theme signal, localStorage persistence
  src-tauri/
    tauri.conf.json       ← decorations:false, shadow:true
    capabilities/default.json
```

**CSS strategy:** global files for tokens/themes/base; **CSS Modules** for components (Vite-native, build-time scoping, zero runtime — satisfies "no CSS-in-JS"). Component CSS may only reference `var(--*)` tokens, never raw hex. Enforce with a stylelint rule (`declaration-property-value-allowed-list` for `color`/`background`).

### 2.2 The token contract (excerpt of `tokens.css`)

Two layers. **Layer A (geometry/elevation)** rarely differs per theme; **Layer B (color/texture)** is what themes override. This split is what makes a future dark "Midnight Aero" a ~6 KB file instead of a fork.

```css
:root {
  /* Layer A — geometry */
  --radius-window: 8px 8px 0 0;   /* Luna overrides; Aero: 6px */
  --radius-control: 3px;
  --titlebar-h: 30px;
  --statusbar-h: 24px;
  --scrollbar-w: 15px;
  --bevel-w: 1px;

  /* Layer B — color/texture contract (defaults = Luna) */
  --font-ui: "Tahoma", "Segoe UI", "DejaVu Sans", sans-serif;
  --font-size-base: 13px;        /* XP was 11px Tahoma; 13px for readability */
  --font-size-chrome: 11px;      /* titlebar/statusbar/meta keep the XP scale */

  --c-window-face: #ece9d8;      /* the Luna beige */
  --c-content: #ffffff;
  --c-text: #1a1a1a;
  --c-text-dim: #6d6d6d;
  --c-bevel-hi: #ffffff;
  --c-bevel-lo: #aca899;
  --c-bevel-dark: #716f64;
  --c-accent: #316ac5;           /* Luna selection blue */
  --c-accent-soft: #c1d2ee;
  --c-titlebar-text: #ffffff;
  --grad-titlebar: linear-gradient(180deg,#0997ff 0%,#0053ee 10%,#0050c9 85%,#1a6fe8 100%);
  --grad-button: linear-gradient(180deg,#ffffff 0%,#f0f0ea 45%,#e3e2d8 100%);
  --grad-selection: linear-gradient(180deg,#3f81e0,#2a5cb8);
  --glass-blur: 0px;             /* Luna has no glass */
  --orb-online: #35c13f;  --orb-idle: #f4b400;  --orb-dnd: #d83b2e;  --orb-off: #9a9a9a;
}
```

`themes/luna.css` and `themes/aero.css` then contain only `[data-theme="luna"] { … }` / `[data-theme="aero"] { … }` blocks overriding Layer B (and a couple of Layer A radii). Aero key values:

```css
[data-theme="aero"] {
  --font-ui: "Segoe UI", "SegoeUI", Tahoma, "Noto Sans", sans-serif;
  --c-window-face: #f0f0f0;            /* Win7 face */
  --c-accent: #2f7fd4;
  --c-accent-soft: #cce8ff;            /* Win7 hover fill */
  --c-accent-border: #99d1ff;          /* Win7 hover border */
  --c-titlebar-text: #15324e;          /* dark text + white glow, Win7-style */
  --grad-titlebar: linear-gradient(180deg, rgba(255,255,255,.78), rgba(214,231,247,.55) 50%, rgba(190,216,240,.6));
  --grad-button: linear-gradient(180deg,#f2f2f2 0%,#ebebeb 49%,#dddddd 51%,#cfcfcf 100%);
  --glass-blur: 14px;                  /* used by .glass-panel via backdrop-filter */
  --radius-window: 6px;
}
```

### 2.3 Runtime switching (SolidJS)

```ts
// src/lib/theme.ts
const [theme, setTheme] = createSignal<Theme>(
  (localStorage.getItem("dice.theme") as Theme) ?? "luna");
createEffect(() => {
  document.documentElement.dataset.theme = theme();
  localStorage.setItem("dice.theme", theme());
});
```

Plus a 3-line inline `<script>` in `index.html` that sets `data-theme` from localStorage **before first paint** (no FOUC). Both theme files are imported statically — ~18 KB raw combined is cheaper than lazy-loading machinery. Theme picker lives in the status bar (a tiny XP-style dropdown: "Luna / Aero").

### 2.4 Dark mode position

Retro Windows chrome is light-first; faking a "dark XP" at M1 would dilute the aesthetic and double QA surface. **Decision: M1 ships Luna + Aero, both light. Post-M1, add `midnight` ("Midnight Aero")** — dark glass (`rgba(20,28,40,.6)` tints, same gloss seam recipe, same geometry tokens). Respect `prefers-color-scheme: dark` only as the *initial default* hint (pick `aero` now, `midnight` once it exists); explicit user choice always wins.

---

## 3. Visual Language

### 3.1 Window chrome (Tauri custom titlebar)

**Tauri config** (`tauri.conf.json`):

```json
"app": { "windows": [{
  "decorations": false, "shadow": true, "transparent": false,
  "resizable": true, "minWidth": 800, "minHeight": 560
}]}
```

`shadow: true` keeps the DWM drop shadow and Win11 rounded corners on the undecorated window. Keep `transparent: false` — transparent windows disable DWM optimizations and cost RAM/CPU.

**Titlebar component:** a 30px header with `data-tauri-drag-region`, app icon, title text, and three caption buttons calling `getCurrentWindow().minimize() / toggleMaximize() / close()` (`@tauri-apps/api/window`). Required capabilities in `capabilities/default.json`: `core:window:allow-start-dragging`, `core:window:allow-minimize`, `core:window:allow-toggle-maximize`, `core:window:allow-close`, `core:window:allow-is-maximized`.

**Windows caveats (write these into the component now, not later):**
- `data-tauri-drag-region` applies **only to the element itself, not children** — put it on the bar *and* on the title `<span>`; never on the button container. Double-click-to-maximize on drag regions is handled by Tauri core.
- **Win11 Snap Layouts flyout will not appear** when hovering our custom maximize button (it's not a native caption button). Accepted M1 loss; note `tauri-plugin-decorum` as a post-M1 option to restore native snap behavior.
- When **maximized**, undecorated Windows windows overflow the work area by the invisible resize border — listen to `onResized`/`isMaximized()` and toggle a `.maximized` class that (a) swaps the maximize glyph to "restore" and (b) zeroes window border-radius and adds compensating padding.
- Keep the top ~5px of the titlebar free of interactive elements so the top **resize handle** still works.

**Luna titlebar look:** `--grad-titlebar` blue gradient, white bold Tahoma 11px title with `text-shadow: 1px 1px 0 rgba(0,0,40,.5)`, top corners rounded 8px. Caption buttons: 21×21px, 3px radius, glossy — min/max in titlebar blue, close in red (`linear-gradient(180deg,#f3a08a,#e25b3d 45%,#c43b1d)`), white glyphs as inline SVG (drawn by us), `box-shadow: inset 1px 1px 0 rgba(255,255,255,.55)` for the gloss, pressed state inverts the bevel.

**Aero titlebar look:** light glass strip (`--grad-titlebar` translucent whites over `--c-window-face`), dark title text with the signature Win7 white glow: `text-shadow: 0 0 8px #fff, 0 0 4px #fff`. Caption buttons are a **joined group** (shared 1px border, only outer corners rounded): min/max 29×19, close 45×19 with red radial hover glow (`radial-gradient(ellipse at 50% 120%, #f9b0a4, #d9492f 65%)`).

**About "real" Aero glass:** `backdrop-filter` inside the WebView blurs *app content behind the element*, not the desktop behind the window. True through-window glass needs the `window-vibrancy` crate (acrylic/mica) — acrylic works on Win11 but the Win7-style blur API lags on resize and costs CPU. **Decision: fake the glass in-app** (translucent gradients + 1px inner white border + optional `backdrop-filter` over app content only). Deterministic, cheap, identical on every OS. Acrylic via `window-vibrancy` is a post-M1 experiment behind a setting.

### 3.2 Typography

| Theme | Stack | Notes |
|---|---|---|
| Luna | `"Tahoma","Segoe UI","DejaVu Sans",sans-serif` | Tahoma ships with Windows since 95; DejaVu covers most Linux distros |
| Aero | `"Segoe UI","SegoeUI",Tahoma,"Noto Sans","Cantarell",-apple-system,sans-serif` | Segoe UI ships with Windows Vista+ |

13px base / 11px chrome & metadata / 15px headings. **No webfonts, ever** (RAM, startup, license — all wins). `font-smoothing` left default; ClearType-era fonts look right with subpixel AA.

### 3.3 Core CSS recipes (in `recipes.css`)

**Raised bevel (Luna buttons, cards):**
```css
.bevel-raised {
  border: var(--bevel-w) solid var(--c-bevel-dark);
  border-radius: var(--radius-control);
  background: var(--grad-button);
  box-shadow: inset 1px 1px 0 var(--c-bevel-hi), inset -1px -1px 0 var(--c-bevel-lo);
}
.bevel-raised:active {        /* pressed = invert + nudge */
  box-shadow: inset 1px 1px 0 var(--c-bevel-lo), inset -1px -1px 0 var(--c-bevel-hi);
  background: linear-gradient(180deg,#e3e2d8,#f0f0ea);
}
.bevel-raised:hover { box-shadow: inset 1px 1px 0 var(--c-bevel-hi),
  inset -1px -1px 0 var(--c-bevel-lo), inset 0 0 0 2px #f8b636aa; } /* XP orange hover ring */
```

**Sunken field (inputs, content wells):**
```css
.bevel-sunken {
  border: 1px solid var(--c-bevel-lo);
  background: var(--c-content);
  box-shadow: inset 1px 1px 2px rgba(0,0,0,.18);
}
```

**The Aero gloss seam** — the entire Win7 look is one trick: a hard gradient stop at the vertical midpoint:
```css
.gloss {
  background:
    linear-gradient(180deg, rgba(255,255,255,.85) 0%, rgba(255,255,255,.45) 49%,
                            rgba(255,255,255,.10) 51%, rgba(255,255,255,.30) 100%),
    var(--c-accent);
  border: 1px solid rgba(0,40,80,.45);
  border-radius: var(--radius-control);
  box-shadow: inset 0 0 0 1px rgba(255,255,255,.6);
}
```

**Glass panel (Aero sidebars/cards):**
```css
.glass-panel {
  background: linear-gradient(160deg, rgba(255,255,255,.55), rgba(205,228,248,.35));
  backdrop-filter: blur(var(--glass-blur));   /* 0px under Luna = no-op, no cost */
  border: 1px solid rgba(255,255,255,.65);
  box-shadow: 0 1px 3px rgba(30,60,90,.18);
}
```

**Status orb (presence):** pure CSS, no images:
```css
.orb { width: 10px; height: 10px; border-radius: 50%;
  background: radial-gradient(circle at 35% 28%, #fff9, var(--orb-c) 45%, color-mix(in srgb, var(--orb-c) 55%, black) 85%);
  box-shadow: inset 0 -1px 1px rgba(0,0,0,.35), 0 0 0 1px rgba(0,0,0,.25);
}
.orb--online { --orb-c: var(--orb-online); } /* etc. */
```
Offline = hollow ring variant (transparent center, 2px ring).

### 3.4 Scrollbars (`scrollbars.css`)

Target engines are WebView2 (Chromium) on Windows and WebKit on future macOS/Linux — **both support `::-webkit-scrollbar`**, including arrow buttons (the standards-track `scrollbar-color` cannot do buttons, so webkit selectors are the right call here).

- **Luna:** 15px wide; track `#f4f3ee` with faint 2px vertical pinstripe (`repeating-linear-gradient`); thumb = horizontal blue gradient `#cde3ff→#9ac2f4`, 1px `#6d94c9` border, inset white highlight, 2px radius, with the classic center grip (3 short lines via tiny inline SVG); `::-webkit-scrollbar-button:single-button` arrow buttons styled as mini beveled buttons with our own 7×4 SVG triangles.
- **Aero:** 13px; flat `#f0f0f0` track; rounded glossy thumb (gloss seam recipe at low alpha); arrow buttons flat until hover (light blue `--c-accent-soft` fill + `--c-accent-border`).

### 3.5 Sound cues (optional flourish)

**Do not ship the real XP "ding"** — it's a copyrighted Microsoft recording. Two clean options: (a) zero-asset WebAudio chime (two sine notes, ~15 lines of JS), or (b) a self-made <10 KB OGG. Behavior: play only for incoming messages **while the window is unfocused**, off by default, toggle in settings. Defer actual implementation; reserve `lib/sounds.ts`.

---

## 4. Per-Screen Treatment (Milestone 1)

### 4.1 Login / Register — "XP Welcome screen"

Full-bleed `--grad-titlebar`-family blue gradient page (Luna) / soft aurora gradient (Aero). Centered two-pane card: left pane = logo + tagline on deep blue; right pane = form on `--c-window-face` with sunken inputs and a default-button (Luna default buttons get the 1px inner `#1c3fbb` ring). Thin white horizontal rules above/below the card, echoing the XP welcome screen's separator lines. Register is the same card, second tab-less route ("New here? Create an account" link styled as classic blue underlined hyperlink).

```
+------------------------------------------------------[X]-+
|  (full-bleed Luna blue gradient, darker at edges)         |
|  =======================================================  |
|                                                           |
|   .--------------------------+------------------------.   |
|   |                          |  Log in to Dice        |   |
|   |      [::] D I C E        |  E-mail                |   |
|   |                          |  [__________________]  |   |
|   |   To begin, enter your   |  Password              |   |
|   |   account details and    |  [__________________]  |   |
|   |   press Log in.          |                        |   |
|   |                          |  [  -> Log in  ]       |   |
|   |   (deep blue panel,      |  New here? Register    |   |
|   |    white text)           |  (beige Luna panel)    |   |
|   '--------------------------+------------------------'   |
|                                                           |
|  =======================================================  |
|   (o) connecting...                            v0.1.0     |
+-----------------------------------------------------------+
```

### 4.2 Main chat screen

```
+=[::]= Dice - Dice HQ / #general ===================[ _ ][ # ][ X ]=+   <- titlebar: Luna gradient /
|--------------------------------------------------------------------|      Aero glass strip
|GR  | CHANNELS            | # general    "memes welcome"     [hdr]  |
|----| ------------------- | ------------------------------------- - |
|(o) | [-] TEXT CHANNELS   |  (o) Ayaan_xp                 10:42  ^  |
| DM | |- # general    <== |   |  did you try the new build?      |  |
|====| |- # dev            |   |  cold start is under 2s now     [#] |
|[A] | '- # off-topic      |                                      |  |
|[B] | [+] VOICE (later)   |  (o) Priya7                   10:43  |  |
|[C] |                     |   |  LETS GOOO                       v  |
|[D] |                     | ------------------------------------- - |
|    | ------------------- | [ Message #general________________][>] |
|[+] | (o) you  | Luna v   |                                         |
|--------------------------------------------------------------------|
| (o) Connected  |  Priya7 is typing...                    14 online |   <- XP status bar
+--------------------------------------------------------------------+
GR = guild rail (taskbar-style)   [#] = scrollbar w/ arrow buttons
```

**Guild rail** — *XP taskbar turned vertical.* 56px column filled with the titlebar gradient (Luna) / glass panel (Aero). Top slot: a **green Start-inspired pill** ("DM" / home) — rounded, glossy green gradient (`#3c9838→#2f8a2b`), white bold label. Below: guild icons as 40×40 quick-launch tiles (rounded 4px). Active guild = **pressed bevel + the XP-orange glow ring** (`inset 0 0 0 2px #f8b636`); unread = small glossy orange notification dot. Bottom: `[+]` add-guild tile. Tooltips on hover styled as classic yellow `#ffffe1` tooltip with 1px black border.

**Channel list** — *Explorer tree (Luna) / Win7 nav pane (Aero).* Panel on `--c-window-face`. Luna: section headers with `[-]`/`[+]` box toggles, 1px dotted `--c-bevel-lo` tree lines, selected channel = solid `--c-accent` bar with white text; hover = `--c-accent-soft`. Aero: chevron triangles instead of boxes, no tree lines, hover/selection = rounded `--c-accent-soft` fill with `--c-accent-border` 1px border (the Win7 hover treatment). Footer: self user strip with presence orb + theme dropdown.

**Message view** — *modern list, retro frame.* The list itself stays clean and modern (avatar-less compact rows, grouped consecutive messages, 13px text, gray 11px timestamps) — readability wins. The retro lives in the frame: Luna wraps the scroll area in a `.bevel-sunken` well on white; Aero floats it as a white rounded card on the glass surface. Day-divider rows styled like classic groupbox legends (text breaking a 1px etched line: `border-top: 1px solid var(--c-bevel-lo); + 1px solid var(--c-bevel-hi)`). Channel header strip = a "rebar" toolbar band with etched bottom edge.

**Composer** — single-line-growing textarea in a `.bevel-sunken` field, white background; Send = `.bevel-raised` default button (Luna, with default ring) / `.gloss` accent button (Aero). Focus state: Luna `outline: 1px dotted #000; outline-offset:-4px` (the classic marquee focus) on buttons, `--c-accent` border on fields.

**Typing indicator** — lives in the **status bar** (see below) as a sunken statusbar cell: `Priya7 is typing` + three dots animated with `animation: dots 1s steps(4) infinite` — *mounted only while someone is typing*, so idle cost is zero. Multiple users collapse to "3 people are typing".

**Status bar** — full-width 24px XP statusbar: `--c-window-face`, cells separated by etched dividers, each cell slightly sunken. Cells: connection orb + "Connected", typing indicator, online count, theme switcher. This is our free "funky" win — Discord has nothing like it and it screams XP.

**Presence orbs** — `.orb` recipe everywhere (member rows, DM list, guild rail self strip): green/yellow/red glossy, gray hollow ring for offline.

**DM list** — same chrome as channel list but rows have orb + name; unread DM = bold name + orange dot (XP "new program installed"-style highlight on hover: soft orange gradient).

---

## 5. Implementation Plan

### Phase 0 — Foundations (0.5 day)
1. `src-tauri/tauri.conf.json`: `decorations:false, shadow:true`; add window-control permissions to `capabilities/default.json`.
2. `styles/tokens.css` + `base.css`; inline pre-paint theme script in `index.html`; `lib/theme.ts` signal.

### Phase 1 — Chrome (1 day)
3. `TitleBar.tsx` with drag region, caption buttons, maximize-state handling (the four Windows caveats from §3.1).
4. `StatusBar.tsx`; `recipes.css` (bevel/gloss/glass/orb); `scrollbars.css`.

### Phase 2 — Themes (1 day)
5. `themes/luna.css`, `themes/aero.css`; theme dropdown in status bar; verify every component renders correctly under both by toggling (this is the regression gate: any hardcoded hex in a component shows up immediately).

### Phase 3 — Screens (2–3 days)
6. LoginCard → GuildRail → ChannelTree → MessageList → Composer → PresenceOrb/TypingIndicator, in that order (each consumes only tokens + recipes).

### CSS budget ledger (unminified targets)

| File | Budget |
|---|---|
| tokens.css | 6 KB |
| base.css | 4 KB |
| recipes.css | 5 KB |
| scrollbars.css | 4 KB |
| luna.css + aero.css | 18 KB |
| 9 component modules | ~28 KB |
| **Total** | **~65 KB raw / ~10 KB gz** (budget: 100 KB) |

Add a CI check: fail the build if `dist/assets/*.css` total exceeds 100 KB.

---

## 6. Performance / Weight Guardrails

- **No raster images.** Gradients for all surfaces; ~10 author-drawn inline SVGs (caption glyphs, scrollbar arrows, tree toggles, send arrow), each < 300 bytes, inlined as data URIs in CSS or JSX.
- **System fonts only.** Zero `@font-face`.
- **No infinite ambient animation.** Gloss/shine is static gradient; transitions (`background-color`, `box-shadow`, ≤150ms) only on interaction. The only loop is the typing-dots `steps()` animation, which exists only while the indicator is mounted. Belt-and-braces: a `visibilitychange`/`blur` listener toggles `html.app-idle { * { animation-play-state: paused !important } }`, and everything respects `prefers-reduced-motion`.
- **`backdrop-filter` discipline:** Aero theme only, at most 2 surfaces (titlebar, guild rail), token-controlled (`--glass-blur`) so a "performance mode" can set it to `0px` with one override. Luna theme uses none.
- **No `transparent: true` window, no window-vibrancy at M1** (both cost RAM/CPU on Windows).
- **Zero runtime styling JS:** CSS Modules + custom properties; theme switch is one `dataset` write.

---

## 7. Risks & Open Questions

| # | Risk / question | Position |
|---|---|---|
| 1 | **Microsoft copyright on XP/7 artwork.** Bitmaps, icons, the Bliss wallpaper, and sound recordings are MS-copyrighted. | "Inspired by, never sampled": all gradients/SVGs authored from scratch; deliberately different hex values and geometry; never extract from Windows DLLs or copy 7.css's PNG data URIs. |
| 2 | **Library licenses.** All three are MIT, but a repo's MIT grant can't launder MS-owned pixels embedded within it (7.css PNG thumbs are the suspect case; XP.css SVGs are hand-redrawn). | We only adapt CSS *techniques* (recipes), credited in THIRD_PARTY_NOTICES. |
| 3 | **Trademark-ish naming.** "Luna" and "Aero" are MS theme codenames. Low risk as internal theme names; if marketing-visible, rename ("Bluebird"/"Frost"). Never market as "Windows XP theme". | Keep as internal IDs for M1; revisit before public launch. |
| 4 | **Win11 Snap Layouts lost** with `decorations:false`. | Accepted for M1; evaluate `tauri-plugin-decorum` later. |
| 5 | **Fonts on future Linux/macOS** — Tahoma/Segoe UI absent. | Fallback stacks specified (§3.2); accept slight metric drift; never bundle MS fonts (license prohibits). |
| 6 | **`backdrop-filter` GPU cost on weak iGPUs.** | Measured in Phase 2; `--glass-blur: 0` escape hatch already designed. |
| 7 | **Open:** does the retro chrome hurt readability for long chat sessions? | Mitigated by keeping the message list modern-clean (§4.2); validate with a dogfood build. |
| 8 | **Open:** accessibility — XP-era 11px chrome text and low-contrast etched dividers. | Base text is 13px; run contrast checks on `--c-text-dim` against both themes; focus states specified for keyboard nav. |

---

### Critical Files for Implementation
- `apps/desktop-client/src/styles/tokens.css` — the token contract every other file depends on
- `apps/desktop-client/src/themes/luna.css` — Luna theme overrides (and template for aero.css/midnight.css)
- `apps/desktop-client/src/styles/recipes.css` — bevel/gloss/glass/orb recipes shared by all components
- `apps/desktop-client/src/components/chrome/TitleBar.tsx` — custom window chrome with the Tauri Windows caveats
- `apps/desktop-client/src-tauri/tauri.conf.json` — `decorations:false`/`shadow:true` window config + capabilities
