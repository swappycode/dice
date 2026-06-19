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

## Deferred

- **Phase 1+ — true hand-off / shared replay.** Durable session identity (persist the
  `resume_token` hash + `next_seq`), then either fetch the buffer from the origin via
  RPC or move the ring to Redis — must preserve seq monotonicity (ADR-0007).

## Verification

- `dice-cache`: `session_directory_records_and_clears_owner` (in-memory) +
  `session_directory_round_trips_advertised_addr` + `session_directory_is_cross_node_over_shared_redis`
  (live Redis, `--ignored`).
- `client_e2e::cross_node_resume_redirects_to_the_owner_address` — seeds the shared
  directory with a remote owner + address and asserts a raw-QUIC `Resume` gets
  `Error{INVALID_SESSION, redirect_addr=<addr>}` (server emit, real QUIC).
- Client splicing: `gateway::tests::{redirect_target_accepts_authorities_rejects_garbage,
  redirected_wss_url_splices_authority, effective_wss_url_follows_a_pending_redirect}`.
- Single-node resume is unchanged (all gateway resume/session + `client_e2e` resume
  tests pass — happy resume + expired resume).
- Manual two-node round trip: run two `dice-monolith` on shared Redis with distinct
  `DICE_NODE_ID` + `DICE_ADVERTISED_ADDR` each. Connect a client to node A, drop the
  connection (open the resume window), then point the reconnect at node B (no sticky
  LB): node B logs `owner_node=A`, increments `dice_gateway_resume_total{outcome="cross_node"}`,
  and replies with A's address; the client reconnects to A and resumes (sees `Resumed`).
