# ADR-0005: Protobuf bodies on REST; sends over the gateway

**Status:** accepted (M1)

Every REST endpoint (including auth) uses `application/x-protobuf` request/response bodies —
one schema language in the entire system, and history responses decode through the exact same
generated code that handles gateway events (one codec path into the client cache). The lost
"curl-ability" of JSON auth bodies was judged not worth a second serialization story.

Split: the **gateway socket** carries high-rate realtime ops (send message with nonce→ack
correlation, typing, presence) — binary, ordered, low latency. **REST** carries request/response
management ops (auth, history backfill, guild/channel/DM CRUD) — retry-safe, middleware-friendly,
per-route rate limits. A REST send endpoint can be added later for bots without protocol changes.
