# ADR-0006: Transactional outbox for the chat message path

**Status:** accepted (M4)

The message path (create / edit / delete) records every dispatch `BusEvent` in an
`event_outbox` table **inside the same transaction as the write**, closing the
commit→publish gap that M1 accepted (see backend-services §12): a process crash or
bus outage between `tx.commit()` and the post-commit publish could lose a fan-out
event, leaving live clients to heal only via resume + REST backfill.

**Shape — optimistic publish + reconciliation.** The write transaction inserts the
message row(s) AND the outbox row atomically. After commit, an inline `bus.publish`
carries the live, low-latency happy path and — on success — stamps the row
`published_at`. A poll-based relay (`chat-service/src/relay.rs`) reconciles any row
the inline publish never stamped: it claims due rows `FOR UPDATE SKIP LOCKED`,
oldest-first, after a short grace window (so it never races the inline path), and
republishes them through the same `EventBus` seam. Delivery is therefore
**at-least-once**, idempotent by `BusEvent.event_id` (and, client-side, by message
id); a published-row sweep bounds table growth.

**Why inline + backstop, not relay-as-sole-publisher.** Keeping the inline publish
preserves today's instant delivery and leaves the six gateway/e2e harnesses (which
expect live fan-out) untouched; the relay is purely the durability backstop and so
sits OFF the live path — which is also why a simple poll suffices and no
`LISTEN/NOTIFY` is required. A pure relay-sole-publisher would be a cleaner single
publish path, but would add poll/notify latency to every message and force a relay
into every test harness.

**Scope.** Message path only. The other ~12 fan-outs (guild / channel / DM / member
/ reaction / read-marker / friend) keep the M1 post-commit publish and heal via
resume + REST backfill; typing is ephemeral and never durable. Extending the outbox
to those sites, gateway-side `event_id` dedup, and multi-node relay election are
follow-ups.

**Relay topology.** Exactly one relay owns each Postgres cluster: the monolith runs
it in-process when chat is local (`!DICE_SPLIT`); in split mode the `chat-service`
bin runs it (it owns the write path). `FOR UPDATE SKIP LOCKED` keeps concurrent
relays correct regardless, so the invariant is a safety property, not a hard
requirement.

**Verification.** `chat-service/tests/outbox.rs` seeds an unpublished row (a dropped
publish) and asserts the relay delivers it exactly once and stamps it published.
Schema: migration `0014_event_outbox.sql`. Works on both the in-process Local bus
and NATS, since the relay republishes through the unchanged `EventBus` trait.
