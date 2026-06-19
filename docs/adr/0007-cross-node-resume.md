# ADR-0007: Cross-node resume — seam extracted, hand-off gated on the seq model

**Status:** accepted (M4) — seam extracted; **phases 0, 0b, 1 and 2b shipped**
(session directory + actionable redirect + durable snapshot + cross-node re-host
with a single-takeover claim). Phase 2a (hand-off RPC) not needed.

ADR-0001 reserved resume as node-local and promised the replay buffer would "sit
behind a trait" so a Redis-backed or hand-off implementation is additive. That
promise was aspirational: `ReplayBuffer` and `ResumeRegistry` were concrete types.
This ADR makes the seam real and records the plan for the rest.

**Done now (behavior-neutral).** `ReplayBuffer` and `ResumeRegistry` are traits
(`services/api-gateway/src/resume.rs`); the in-memory single-node types are
`LocalReplayBuffer` / `LocalResumeRegistry`. `SessionState.replay` is a
`Box<dyn ReplayBuffer>` and `Gateway.resume` is a `Box<dyn ResumeRegistry>`, so an
alternative buffer/registry drops in without touching the session task, dispatcher,
or handshake. No behavior changes: resume is still node-local and a cross-node
`Resume` still fails cleanly as `INVALID_SESSION` → REST backfill.

**The hard part is the per-node seq model, not the buffer storage.** Seq is assigned
per-session by a single-writer task at delivery time (`SessionState.prepare`,
`next_seq`), and the replay ring lives inside that task. A naive "put the ring in
Redis" is insufficient: if a client reconnects to another node, that node has no seq
context for the session and would restart seq at 1, breaking the client's strict
monotonic-seq invariant. So cross-node resume needs seq *continuity*, not just frame
*storage*.

**Plan (in order), each gated behind this ADR before shipping:**
1. **Phase 0 — sticky-LB affinity.** The load balancer routes a reconnect (keyed by
   `gateway_session_id`) back to the owning node within the resume window. The
   detached session task is still alive there, so resume works unchanged — no seq
   coordination, ~80% of the scaling story for ~20% of the effort. Cost: if the
   owning node dies mid-window the session is unrecoverable except via REST backfill
   (acceptable; document the operator requirement). **Done** — a `Resume` that misses
   the local registry consults `dice_cache::SessionDirectory`
   (`resume:owner:{session_id}` → owner node id, TTL = resume window) and emits
   `dice_gateway_resume_total{outcome=resumed|cross_node|gone}`.
   **Phase 0b — actionable redirect (DONE).** The directory value also carries the
   owner's advertised `host:port` (`DICE_ADVERTISED_ADDR`, recorded in `detached_wait`).
   A cross-node `Resume` then gets `Error{INVALID_SESSION, redirect_addr=<owner addr>}`
   (proto field on `dice.v1.Error`); the client reconnects to that address and retries
   Resume, keeping its caches (network-core driver, bounded by `MAX_REDIRECTS`). No
   sticky LB required when nodes advertise an address; the connection stays open
   (protocol §3) so a client that ignores the redirect still falls back to a fresh
   Identify. The replay ring is never moved — that is phase 1+.
2. **Phase 1 — durable session identity (DONE).** On detach, the session task (the
   single seq writer) persists a `ResumeSnapshot {user, auth_session, resume_token,
   next_seq, trimmed_to, frames[]}` to the shared `Cache` (`resume:snapshot:{session_id}`,
   TTL = resume window) — so any node can validate the token + learn the last seq
   without a live task on the origin. The `resume:owner` directory entry becomes a
   short **lease** (TTL = ½ window, refreshed every ¼ window) so a *dead* owner's
   lease expires within the window (the trigger to re-host) while a *live* owner's
   stays fresh (the trigger to redirect, phase 0b).
3. **Phase 2b — shared replay / re-host (DONE).** The ring moves to the shared cache
   on detach (above) and re-hydrates elsewhere: a `Resume` that misses the local
   registry AND finds no live owner lease calls `try_rehost` — constant-time token
   check + coverage, then a **single-takeover claim** (`Cache::incr_expire == 1`, the
   only atomic single-winner primitive available — no SET-NX) so two nodes never
   re-host the same session, then `handshake::rehost` re-subscribes the user's world,
   rehydrates `LocalReplayBuffer::from_snapshot`, continues seq from `next_seq`, and
   sends `Resumed` + replay. **Seq monotonicity holds because a detached client
   receives nothing:** seqs the origin assigns AFTER the snapshot were never
   client-visible, so a re-host continuing from `next_seq` cannot regress the client
   (the gap of post-snapshot events is recovered via REST backfill). `next_seq` is
   additionally floored at `last_ring_seq + 1`. **Death while ATTACHED** (no snapshot
   yet) still degrades to a fresh Identify + REST backfill — accepted. Phase 2a
   (hand-off RPC from a live origin) was not needed: phase 0b already redirects to a
   live origin, so re-host targets the *dead*-origin case directly.

**Idempotency note.** `BusEvent.event_id` is the only cross-service dedup key; seq is
per-session and never global. The transactional outbox (ADR-0006) gives the
durable-publish floor any multi-node topology needs, so resume sits on solid ground.

**Verification.** Automated, in-process (`crates/network-core/tests/client_e2e.rs`):
`cross_node_rehost_replays_from_a_snapshot` seeds a durable snapshot and asserts a
raw-QUIC `Resume` re-hosts + replays the ring in seq order; `detach_persists_a_durable_snapshot`
asserts the snapshot is written on detach; `cross_node_resume_redirects_to_the_owner_address`
covers the phase-0b redirect; plus serialization + claim unit tests (`durable.rs`) and
the unchanged same-node resume tests. Full manual: two `dice-monolith` nodes on shared
NATS + Redis + Postgres; connect to A, send messages, kill A, resume on B (the lease
expires within the window → B re-hosts). Only the 100k throughput gate needs real HW.
