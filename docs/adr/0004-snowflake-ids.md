# ADR-0004: Snowflake IDs with bit 63 = 0 and a 2026 epoch

**Status:** accepted

`[1 bit 0][41 bits ms since 2026-01-01T00:00:00Z][10 bits node id][12 bits sequence]` — good to
~2095. Bit 63 always 0 so every id fits Postgres `BIGINT`, Rust `i64`, and JS `BigInt` without
sign games (and the client stores them as SQLite INTEGER safely). Wire form is `fixed64`
(snowflake high bits are timestamp, varint would cost 9–10 bytes). Human/REST form: decimal string.

Generated server-side only, at the owning service (auth → user ids, chat → guild/channel/message,
gateway → gateway-session ids, bus → event ids). Clients never mint ids; they use the `Frame.nonce`
field for request correlation. Generator: lock-free CAS on one `AtomicU64` packed as
`(timestamp << 12) | seq` in `dice-common`; node id from `DICE_NODE_ID` (0–1023, default 0).
Embedded timestamp eliminates `created_at` columns and wire fields for messages.
