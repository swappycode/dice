# Cross-node resume — phase 0/0b (session directory + actionable redirect)

Design for the first slice of multi-node gateway resume (ADR-0007). Resume itself
stays node-local; this adds the routing substrate so a reconnect that lands on a
different gateway node can be sent back to the node that still owns the detached
session — within the resume window — without moving any replay state. **Phase 0**
records ownership + relies on a sticky LB; **phase 0b** makes the redirect
machine-actionable (the client reconnects to the owner's advertised address itself).

## Why this is phase 0

Resume's hard problem is the per-node seq model: seq is assigned per session by a
single-writer task and the replay ring lives inside that task (ADR-0007). Phase 0
sidesteps it by keeping a reconnect ON the owning node, so resume runs unchanged.
The codebase contribution is therefore not a buffer rewrite but a **session→node
directory** that routing can consult.

## The session directory

`dice_cache::SessionDirectory` over the shared `Cache`: `resume:owner:{session_id}`
→ the owning gateway's `u16` node id, optionally followed by that node's advertised
`host:port` (for the phase-0b redirect), TTL = the resume window.

- **Populated** when a session enters its resume window (`session::detached_wait`):
  the owning node records itself. **Cleared** on every exit (resumed locally, window
  expired, revoked, torn down). So the directory holds exactly the set of currently
  detached, resumable sessions, each expiring precisely when it stops being
  resumable.
- **Consulted** on a `Resume` that misses the *local* `ResumeRegistry`
  (`session::drive_connection`): if another node owns it, the gateway emits
  `dice_gateway_resume_total{outcome="cross_node"}` + an info log and returns
  `INVALID_SESSION` with a message telling the client to reconnect via the sticky
  LB; otherwise `outcome="gone"` and the usual fresh-Identify path. A successful
  local resume records `outcome="resumed"`.
- Backed by any `Cache`: genuine cross-node only with shared Redis; with the
  in-memory backend it is per-process and harmless (single-node deployments never
  need it). Proven by `session_directory_is_cross_node_over_shared_redis`.

It sits behind the `ResumeRegistry`/`ReplayBuffer` traits extracted in M4 (7/n): the
directory is additive, and a future `SharedResumeRegistry` can fold it in.

## Routing: who acts on the directory

Phase 0 expects a **sticky load balancer** keyed on `gateway_session_id`
(cookie/hash) to route reconnects back to the owning node within the window — then
resume just works (the detached task is still there). The directory + the
`cross_node` metric give operators the signal (affinity misses) and a future control
plane the data to route on.

**Cost / accepted limitation:** if the owning node dies mid-window, the session is
unrecoverable except via REST history backfill — acceptable for phase 0 (document
the operator requirement; phase 1 adds durable session identity).

## Phase 0b — actionable redirect (done)

When the owning node sets `DICE_ADVERTISED_ADDR`, it records that `host:port` in the
directory alongside its node id. A cross-node `Resume` miss then replies
`Error{INVALID_SESSION, redirect_addr=<owner addr>}` — a new field on `dice.v1.Error`
(no new frame; the connection stays open per protocol §3). The network-core client
driver, on a resuming `INVALID_SESSION` carrying a `redirect_addr`, splices that
authority into its transport target (WSS URL host/port + the QUIC dial target), keeps
its resume state, and retries `Resume` against the owner — no `SessionInvalidated`,
caches stay valid. A bounded counter (`MAX_REDIRECTS`) stops a misconfigured directory
from bouncing a client forever; past it the client degrades to a fresh Identify. A
node that advertises no address keeps the phase-0 behaviour (sticky LB routes it).

This sits behind the same `ResumeRegistry`/`ReplayBuffer` traits (M4 7/n): the redirect
is additive and a future `SharedResumeRegistry` can fold it in.

## Phase 1 + 2b — durable snapshot + re-host (done)

Phase 0b sends a reconnect back to a *live* owner. Phase 2b survives the owner's **death**:
a different node re-hosts the session from a durable snapshot.

**The directory entry is now a lease.** `detached_wait` records `resume:owner:{sid}` with a
short TTL (½ window) and re-records it every ¼ window. A live owner keeps it fresh (→ phase-0b
redirect); a dead owner's lease expires within the window (→ re-host). The snapshot
(`resume:snapshot:{sid}`, TTL = full window) carries the identity + `next_seq` + `trimmed_to`
+ the serialized ring; it is refreshed on the same tick. Both are cleared on a clean exit.

**Write side** (`session.rs`): `snapshot_of` captures the ring from the single-writer session
task; `persist_snapshot` saves it via `durable::DurableResume`. Hand-rolled length-prefixed
encoding (no schema churn), decode is total (malformed → `None` → fresh Identify).

**Read side** (`session.rs::try_rehost` → `handshake::rehost`): on a local-miss `Resume` with no
live owner lease — load the snapshot, constant-time token check + coverage (`last_seq >=
trimmed_to`), then win a **single-takeover claim** (`Cache::incr_expire == 1`, the only atomic
single-winner primitive — the `Cache` trait has no SET-NX/CAS) so two nodes never re-host the
same session. The winner re-derives the user's subscriptions (`sync_user_state` → presence +
router), rehydrates `LocalReplayBuffer::from_snapshot`, `ack(last_seq)`, continues seq from
`next_seq` (floored at `last_ring_seq + 1`), and sends `Resumed` + replay.

**Why snapshot-on-detach is seq-safe.** A *detached* client receives nothing, so any seq the
origin assigns AFTER the snapshot was never client-visible. A re-host continuing from the
snapshot's `next_seq` therefore cannot regress or duplicate a seq the client already saw; the
gap of post-snapshot events is recovered via REST backfill. **Death while ATTACHED** (no
snapshot yet) still degrades to a fresh Identify — accepted.

**Cost / accepted limitations.** Re-host detection lags by up to ½ window (the lease TTL). The
periodic lease+snapshot refresh is one small `Cache` write per detached session per ¼ window.
A re-host that crashes after winning the claim leaves the claim until its TTL (re-host blocked
for the rest of the window — rare). The owner's still-alive detached task keeps buffering with
no transport until it expires (no client-visible double-send).

## Deferred

- **Phase 2a — hand-off RPC** from a *live* origin. Not built: phase 0b already redirects to a
  live origin, so re-host targets the dead-origin case directly.
- **Death-while-attached full replay.** Would need periodic snapshots of attached sessions —
  deliberately avoided (hot-path cost); REST backfill covers it.

## Verification

- `dice-cache`: `session_directory_records_and_clears_owner` (in-memory) +
  `session_directory_round_trips_advertised_addr` + `session_directory_is_cross_node_over_shared_redis`
  (live Redis, `--ignored`).
- `client_e2e::cross_node_resume_redirects_to_the_owner_address` — seeds the shared
  directory with a remote owner + address and asserts a raw-QUIC `Resume` gets
  `Error{INVALID_SESSION, redirect_addr=<addr>}` (server emit, real QUIC).
- `client_e2e::cross_node_rehost_replays_from_a_snapshot` — seeds a durable snapshot
  (the dead origin's ring) and asserts a raw-QUIC `Resume` re-hosts + replays the ring
  in seq order over real QUIC; `detach_persists_a_durable_snapshot` asserts the write side.
- `api-gateway durable::tests` — snapshot encode/decode round-trip (incl. empty ring +
  truncated/garbage → `None`) + `try_claim` admits exactly one winner.
- Client splicing: `gateway::tests::{redirect_target_accepts_authorities_rejects_garbage,
  redirected_wss_url_splices_authority, effective_wss_url_follows_a_pending_redirect}`.
- Single-node resume is unchanged (all gateway resume/session + `client_e2e` resume
  tests pass — happy resume + expired resume).
- Manual two-node round trip: run two `dice-monolith` on shared Redis with distinct
  `DICE_NODE_ID` + `DICE_ADVERTISED_ADDR` each. Connect a client to node A, drop the
  connection (open the resume window), then point the reconnect at node B (no sticky
  LB): node B logs `owner_node=A`, increments `dice_gateway_resume_total{outcome="cross_node"}`,
  and replies with A's address; the client reconnects to A and resumes (sees `Resumed`).
