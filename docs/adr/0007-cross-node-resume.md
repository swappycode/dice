# ADR-0007: Cross-node resume — seam extracted, hand-off gated on the seq model

**Status:** accepted (M4) — seam extracted; cross-node delivery deferred

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
   (acceptable; document the operator requirement).
2. **Phase 1 — durable session identity.** Persist `(session_id, user, resume_token
   hash, node_id, next_seq, expires_at)` (or a Redis KV with TTL = resume window) so
   any node can validate the token and learn the last assigned seq without a live
   task on the origin node.
3. **Phase 2 — hand-off OR shared replay.** Either (a) the resuming node fetches the
   buffer + seq state from the origin via NATS RPC and continues, or (b) the ring
   moves to Redis on detach and re-hydrates elsewhere. Both must preserve seq
   monotonicity (gate new dispatches until replay completes).

**Idempotency note.** `BusEvent.event_id` is the only cross-service dedup key; seq is
per-session and never global. The transactional outbox (ADR-0006) gives the
durable-publish floor any multi-node topology needs, so resume sits on solid ground.

**Verification (when built).** Fully local: two `dice-monolith` nodes on shared
NATS + Redis + Postgres; connect to node A, send messages, kill node A, resume on
node B. Only the 100k-connection throughput gate needs real hardware.
