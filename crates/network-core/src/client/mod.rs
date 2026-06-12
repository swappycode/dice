//! Client half (feature `"client"`): everything the desktop host needs to
//! talk to a Dice gateway over QUIC (primary) or WSS (fallback), per
//! docs/design/desktop-client.md §1.
//!
//! - [`tls`](mod@tls): [`TlsOptions`] — webpki roots plus optional extra
//!   anchors (the dev CA via `DICE_DEV_CA`). Full verification always runs
//!   on BOTH transports; there is deliberately NO verification-off switch.
//! - [`transport`]: [`WssTransport`] / [`AnyTransport`] — one binary WS
//!   message = one bare `dice.v1.Frame`.
//! - [`quic`](mod@quic): [`QuicTransport`] — the single client-opened bidi
//!   control stream, `u32`-BE length-prefixed frames, ALPN `dice/1`.
//! - [`policy`]: [`TransportPolicy`] / [`PreferredTransport`] — QUIC-first
//!   with a 3 s budget, WSS fallback in the same attempt, WSS preference
//!   after 2 consecutive QUIC failures with periodic re-probes.
//! - [`gateway`]: the driver task — transport selection, Identify/Resume
//!   handshake, heartbeats, full-jitter backoff — behind [`GatewayHandle`].
//! - [`api`]: [`ApiClient`] — protobuf-over-HTTPS REST per
//!   docs/protocol.md §10.

pub mod api;
pub mod gateway;
pub mod policy;
pub mod quic;
pub mod tls;
pub mod token;
pub mod transport;

pub use api::{ApiClient, ApiError};
pub use gateway::{
    ClientEvent, Command, ConnState, ConnStateLite, GatewayClientConfig, GatewayHandle, SendError,
    connect,
};
pub use policy::{DEFAULT_QUIC_TIMEOUT, PreferredTransport, TransportPolicy};
pub use quic::{InvalidQuicEndpoint, QuicAddr, QuicEndpoint, QuicTransport};
pub use tls::TlsOptions;
pub use token::{TokenError, TokenProvider};
pub use transport::{AnyTransport, ClientTransportError, TransportKind, WssTransport};

// Consumers take these from here so versions can never skew from the ones
// the client half was built against.
pub use reqwest;
pub use url;
