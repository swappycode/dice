# ADR-0008: No guild sharding — NATS subjects + cross-node resume cover the scale

**Status:** accepted (M4) — guild/connection sharding is deliberately NOT built.

The M4 roadmap listed "guild sharding + lazy member lists" as a scaling item.
Lazy member lists shipped (M4 5/n + 9/n). This ADR records the decision **not**
to build guild *sharding*, because Dice's architecture makes it inapplicable.

## What "guild sharding" would mean, and why it doesn't apply

**Discord's model (bot shards).** A Discord bot opens N gateway connections, each
owning guilds where `(guild_id >> 22) % N == shard_id`, because one socket can't
carry the fan-out of a large bot's guilds. This is a **bot** concern: a single
process subscribing to thousands of guilds.

Dice is a **user client**, not a bot. A user connects **once** and subscribes only
to the guilds + DMs they are a member of — a bounded, small set. There is no
single connection under fan-out pressure, so client-side sharding solves a problem
Dice does not have. (Discord itself never shards *user* clients.)

**Server-side guild partitioning** — pinning each guild's fan-out to a specific
backend node — is also unnecessary here: fan-out already rides **NATS subjects**
(`Subject::GuildMsg(guild_id)`, `Subject::GuildVoice`, …). Any gateway node
subscribes to exactly the subjects its connected users need, and NATS distributes
publication to all interested subscribers across the cluster. Adding a guild→node
assignment layer on top would duplicate what the subject fabric already does, and
would *re-introduce* the cross-node coupling that the subject model avoids.

## Why the scale target is already met without it

The scaling envelope is covered by work that shipped:

- **Vertical:** one gateway node sustains **100k concurrent connections** at
  ~44 KB/conn (M4 10/n, measured) — far past where Discord would shard a bot.
- **Horizontal:** the gateway runs as **many independent nodes** (StatefulSet),
  each subscribing to its users' subjects over the shared NATS bus; presence is in
  shared Redis; the transactional outbox (ADR-0006) gives durable fan-out.
- **Resilience across nodes:** **cross-node resume** (ADR-0007 phases 0/0b/1/2b) —
  a reconnect that lands on the wrong node is redirected to the owner, and a node's
  *death* is recovered by re-hosting the session from a durable snapshot on another
  node. This is the property guild sharding would otherwise be reached for.
- **Per-guild cost:** **lazy member lists** (RequestGuildMembers paging) +
  on-demand user fetch keep a large guild's `Ready` bounded, which was the real
  large-guild pressure point.

## Consequence

Scaling out is "add gateway nodes behind the LB" — no shard-assignment service, no
client-side multi-connection complexity, no guild→node rebalancing. If a future
deployment ever exceeds what NATS-subject fan-out + 100k/node can carry, the
re-examination starts from **NATS superclusters / leaf nodes** (an infra change),
not from client or guild sharding (an architecture change). That is intentionally
deferred until a measurement demands it.
