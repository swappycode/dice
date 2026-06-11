//! Transport plumbing for the Dice gateway (server half) and the desktop
//! client (client half).
//!
//! - [`tls`] (shared, not feature-gated): ring-only rustls config builders,
//!   dev CA/leaf generation, and quinn server-config tuning per
//!   docs/protocol.md §1.
//! - [`server`] (feature `"server"`): the [`server::FramedTransport`] seam the
//!   gateway codes against, the QUIC acceptor/transport, and the hand-rolled
//!   tokio-rustls + hyper HTTPS accept loop (REST + WS upgrade share one port;
//!   no axum-server, per critique resolution #20).
//! - [`client`] (feature `"client"`): the desktop client's transport
//!   ([`client::AnyTransport`], WSS-only in this phase — QUIC is the Phase-4
//!   slot), the gateway driver (Identify/Resume, heartbeat, full-jitter
//!   backoff) and the protobuf-over-HTTPS [`client::ApiClient`].
//!
//! The wire contract is docs/protocol.md (normative). The ONE framing codec
//! lives in `dice-protocol::framing` and is consumed — never reimplemented —
//! here.

pub mod tls;

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "client")]
pub mod client;

// Consumers (api-gateway, monolith) take quinn/rustls types from here so the
// versions used to build configs can never skew from the ones used to serve.
pub use quinn;
pub use rustls;
pub use tokio_rustls;
