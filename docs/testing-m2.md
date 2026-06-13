# Dice — Milestone 2 manual test guide

How to bring the system up and exercise every M2 feature by hand, with the exact
commands, where each value/token shows up, and the things that *won't* work yet.
The authoritative changelog is `WORKLOG.md` (`M2 (1/n)…(9/n)`); this is the
"drive it yourself" companion. Shell snippets are PowerShell (the repo's
`justfile` uses it).

---

## 0. One-time setup

```powershell
cp .env.example .env        # gitignored; loaded by just/sqlx
just infra-up               # docker: Postgres :5433, Redis :6379, NATS :4222 (waits for health)
```

Postgres is **always** required. Redis + NATS are only needed for the `full`
profile and the split-mode RPC tests.

The server auto-generates dev TLS (`dev/certs/`) and JWT keys (`dev/keys/`) on
first boot. The client trusts the dev CA via `DICE_DEV_CA` — the `just client*`
recipes set it for you.

### Profiles

| Profile | Bus | Cache | Use |
|---|---|---|---|
| `dev-lite` (default) | in-process | in-memory | fast loop; **everything in this guide works** except live unread *counts* |
| `full` | NATS | Redis | production-shaped; needed for the durable unread-count consumer |

> Unread **badges/counts** come from the durable JetStream consumer
> (`notification-service`), which only runs under `full`. In `dev-lite`, unread
> accrues client-side and the **chime/toast still fire** (they key off the live
> `messageCreate` dispatch, not the counter).

### Run

```powershell
just dev            # server, dev-lite        (or: just run-full)
just client         # desktop client, HMR dev build (separate terminal)
```

For the two-user flows (chime/toast, live chat) use built clients with isolated
profiles:

```powershell
just client-build          # once: vite build + release exe
just client-as alice       # terminal A
just client-as bob         # terminal B
```

---

## 1. TOTP 2FA (item 11a)

Open the **🔒 button** in the bottom-left **SelfStrip** → the **Account
security** dialog.

**Enroll**
1. Click **Set up 2FA**. You'll see a **Setup key** (base32, 160-bit secret) and
   an **Open in authenticator** link (`otpauth://totp/Dice:<username>?…`,
   SHA-1 / 6-digit / 30 s). There is **no server-rendered QR image** by design —
   paste the setup key into your app, or click the link.
2. Add it to any RFC 6238 app (Google Authenticator, Aegis, 1Password, …).
3. Enter the 6-digit code → **Verify**.
4. **10 one-time recovery codes** appear — save them — then **Done**.

**Login challenge** — log off, log in again: after the password step you get a
**Two-step verification** screen. Enter the current 6-digit code → in. The same
code won't work twice (single-use replay guard); wait for the next 30 s window.

**Recovery code path** — at the challenge screen, type a recovery code instead of
the TOTP. It logs you in and is then consumed (reusing it fails).

**Disable** — 🔒 → **Turn off 2FA** → a current TOTP *or* recovery code.

---

## 2. Email verification + password reset (item 11b)

There is **no SMTP**. The dev `LogMailer` *logs* the mail (token included) to the
**server console** — copy the token from there. Default `.env`
(`RUST_LOG=info,dice=debug`) already shows it. Look for:

```
INFO … LogMailer: would send mail
to: <email>  subject: Verify your Dice email
Welcome to Dice!

Your verification token:

  dvt_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx

Enter it under Security → Verify email. It expires in 24 hours.
```

- **Verify token** is prefixed `dvt_` (expires **24 h**).
- **Reset token** is prefixed `drst_` (expires **30 min**).

**Verify email** — register a new account (a `dvt_` token is logged immediately),
then 🔒 → **Verify email** → paste the token → **Verify** ("Email verified ✓").
**Resend email** logs a fresh `dvt_`.

**Password reset** — on the login screen click **Forgot your password?** →
enter the email → **Send reset code** (a `drst_` token is logged; the response is
always 204, so an unknown email reveals nothing). Then paste the code + a new
password → **Reset password**. Note: a successful reset **revokes every existing
session** for that account, so other logged-in devices drop to the login screen.

---

## 3. Theme pack + funk pass (items 12, 13)

**StatusBar → Theme** dropdown. Six themes ship (all real CSS, persisted to
localStorage): **Luna**, **Aero** (M1 light), and the M2 dark/funk pack —
**Midnight** (smoked-glass + ice-cyan), **Nocturne** (charcoal + magenta neon),
**Bubble** (Y2K aqua), **Phosphor** (CRT green).

- Switch through all six; the whole UI recolors instantly (`[data-theme]`).
- **Phosphor**: note the faint static **scanline veil** over everything. Tick the
  **Perf** checkbox (next to the dropdown) → the veil disappears and glass blur
  drops to 0 (GPU escape hatch). Untick to restore.
- **Funk pass (item 12)**: hover any button — the bevel/ring eases in (~120 ms)
  instead of snapping; keyboard-focus the default button (e.g. **Log in**) — it
  blooms a held accent glow. Both honor `prefers-reduced-motion`.

---

## 4. Chime + OS toast (item 14)

Fires when a new message lands **outside the channel you're actively viewing**,
or while the **window is in the background**. Author's own messages never notify.

Two-user test (built clients):
1. `just dev`, then `just client-as alice` and `just client-as bob`; both join the
   same guild.
2. Alice views channel **A**; Bob posts in channel **B** → Alice hears the
   synthesized two-note **chime** (Web Audio, no asset; throttled to 1/1.5 s).
3. Alice minimizes / focuses another app; Bob posts again → a **Windows OS toast**
   appears (author + message snippet). Toast only fires while unfocused.

Caveats: in **browser mock** (`npm run dev`) the chime plays but the toast no-ops
(no Tauri host). On Windows, the first toast may need notification permission for
the app. Audio needs a prior user gesture (always true post-login).

---

## 5. Split-mode NATS RPC (item 15)

**What ships:** the generic `dice-event-bus::rpc` request-reply layer + **Presence
fully over NATS** (`PresenceNatsClient` behind `dyn Presence`, `rpc::serve`
responder). **What does NOT ship yet:** runnable split *processes* — there are no
`services/*/src/bin/*.rs` bins, and Auth/Chat aren't wired over RPC. So you can't
literally run auth/chat/presence as separate daemons; the monolith is still the
only server. The split mode is **proven by two live NATS tests** (self-contained —
they spin their own responder + client; you do **not** need the monolith running,
only NATS):

```powershell
just infra-up   # NATS on :4222 (if not already up)

cargo test -p dice-event-bus rpc::                 # generic ok/fault round-trip
cargo test -p presence-service --test presence_rpc # full Presence vertical over NATS
```

Both print `test result: ok`. They **skip cleanly** (not fail) if NATS is down.

---

## 6. Full quality gates (what CI / a reviewer runs)

```powershell
just check                              # fmt --check + clippy -D warnings + cargo test (incl. live tests) + aws-lc gate

cd apps/desktop-client/src-tauri
cargo clippy --all-targets -- -D warnings
cargo test                              # 15 lib + 2 host_gate

cd ../                                  # apps/desktop-client
npm run check                           # tsc --noEmit
npm run build                           # vite production bundle

# only when you change SQL (sqlx::query! macros) or add a migration:
just sqlx-prepare                       # applies migrations + regenerates .sqlx (commit it)
```

`just check` needs infra up (Postgres + NATS): many tests are live-infra
integration tests.

---

## 7. Quick verification checklist

- [ ] 2FA: enroll → authenticator syncs → login challenge → recovery-code login → disable
- [ ] Email verify: `dvt_` token from log → verified ✓; resend issues a new one
- [ ] Password reset: `drst_` token from log → new password works, old sessions dropped
- [ ] All 6 themes render; Phosphor veil toggles with Perf
- [ ] Funk: smooth hover feedback + default-button focus glow
- [ ] Chime on background/other-channel message; OS toast when unfocused (built client)
- [ ] `cargo test -p dice-event-bus rpc::` and `-p presence-service --test presence_rpc` pass
- [ ] `just check`, host clippy+tests, `npm run check` + `build` all green
