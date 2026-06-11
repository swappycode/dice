# ADR-0001: QUIC primary, WSS fallback, one envelope and one codec

**Status:** accepted (M1)

QUIC (quinn, ALPN `dice/1`) is the primary realtime transport: lower latency, multiplexed
streams for future voice, better mobile/NAT behavior. Secure WebSocket is the fallback for
UDP-hostile networks. Both carry the identical `dice.v1.Frame` envelope and state machine; the
only difference is framing (u32-BE length prefix on the QUIC control stream vs self-delimiting
binary WS messages), implemented once in `dice-protocol::framing`.

One ordered QUIC control stream (not stream-per-message): resume requires a per-session total
order, and per-stream overhead per chat message wastes the per-connection memory budget. QUIC
datagrams are reserved (capability bit 0) for genuinely loss-tolerant traffic later (typing,
voice); cut from M1.

Resume is node-local (replay ring buffer on the gateway node; 256 frames/256 KiB, 60 s window).
Cross-node resume deliberately fails as INVALID_SESSION; the buffer sits behind a trait in the
session struct so a Redis-backed or hand-off implementation is additive post-M1.
