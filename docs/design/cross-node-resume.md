# Cross-node resume — phase 0 (session directory + sticky-LB affinity)

Design for the first slice of multi-node gateway resume (ADR-0007). Resume itself
stays node-local; this adds the routing substrate so a reconnect that lands on a
different gateway node can be sent back to the node that still owns the detached
session — within the resume window — without moving any replay state.

## Why this is phase 0

Resume's hard problem is the per-node seq model: seq is assigned per session by a
single-writer task and the replay ring lives inside that task (ADR-0007). Phase 0
sidesteps it by keeping a reconnect ON the owning node, so resume runs unchanged.
The codebase contribution is therefore not a buffer rewrite but a **session→node
directory** that routing can consult.

## The session directory

`dice_cache::SessionDirectory` over the shared `Cache`: `resume:owner:{session_id}`
→ the owning gateway's `u16` node id, TTL = the resume window.

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

## Deferred

- **Phase 0b — actionable redirect.** Today the cross-node `INVALID_SESSION` carries
  a human-readable message; a machine-actionable redirect (a proto field or a
  `ResumeRedirect` frame carrying the owner's reachable address, which the client
  reconnects to) needs a protocol + client change and a per-node advertised address.
- **Phase 1+ — true hand-off / shared replay.** Durable session identity (persist the
  `resume_token` hash + `next_seq`), then either fetch the buffer from the origin via
  RPC or move the ring to Redis — must preserve seq monotonicity (ADR-0007).

## Verification

- `dice-cache`: `session_directory_records_and_clears_owner` (in-memory) +
  `session_directory_is_cross_node_over_shared_redis` (live Redis, `--ignored`).
- Single-node resume is unchanged (all gateway resume/session + `client_e2e` resume
  tests pass).
- Manual two-node: run two `dice-monolith` on shared Redis with distinct
  `DICE_NODE_ID`; connect to node A, drop the connection, point a reconnect at node B
  with the same `Resume` → node B logs `owner_node=A` and increments
  `dice_gateway_resume_total{outcome="cross_node"}`.
