//! Hand-rolled HTTPS accept loop: tokio-rustls + hyper http1 serving an axum
//! `Router` on one TLS TCP port for both REST and the `/gateway/v1` WebSocket
//! upgrade. Deliberately no axum-server (critique resolution #20).

use std::net::SocketAddr;
use std::sync::Arc;

use hyper::service::Service as _;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

/// The TLS peer's socket address, injected into every request's extensions by
/// the accept loop. Handlers read it (via `axum::Extension<PeerAddr>`) for
/// per-IP logic such as auth rate limiting. This is the REAL socket peer —
/// `X-Forwarded-For` is deliberately NOT trusted (no proxy in front yet).
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}

/// Bind `addr` and serve `router` over TLS until `ct` is cancelled.
///
/// Graceful shutdown: cancelling stops *accepting* and returns; in-flight
/// connections (including long-lived WS sessions) are left to finish — the
/// gateway closes sessions itself with `Close{GOING_AWAY}`.
pub async fn serve_https(
    addr: SocketAddr,
    tls: Arc<rustls::ServerConfig>,
    router: axum::Router,
    ct: CancellationToken,
) -> Result<(), ServeError> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| ServeError::Bind { addr, source })?;
    serve_https_on(listener, tls, router, ct).await
}

/// [`serve_https`] over an already-bound listener (lets callers/tests bind
/// port 0 and learn the address first).
pub async fn serve_https_on(
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    router: axum::Router,
    ct: CancellationToken,
) -> Result<(), ServeError> {
    let acceptor = TlsAcceptor::from(tls);
    loop {
        let (stream, peer) = tokio::select! {
            () = ct.cancelled() => return Ok(()),
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(err) => {
                    // Transient (per-connection) accept errors must not kill
                    // the listener; resource exhaustion heals as conns close.
                    tracing::warn!(error = %err, "TCP accept failed");
                    continue;
                }
            },
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(err) => {
                    tracing::debug!(%peer, error = %err, "TLS handshake failed");
                    return;
                }
            };
            // Inject the peer address into every request's extensions before
            // handing off to axum, so REST handlers can do per-IP rate limiting.
            // WS-upgrade requests carry it too (currently unused there).
            let inner = TowerToHyperService::new(router);
            let service =
                hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                    req.extensions_mut().insert(PeerAddr(peer));
                    inner.call(req)
                });
            // `with_upgrades` is what lets `axum::extract::ws` complete the
            // WebSocket upgrade on this hand-rolled stack.
            let conn = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(tls_stream), service)
                .with_upgrades();
            if let Err(err) = conn.await {
                tracing::debug!(%peer, error = %err, "HTTP connection ended with error");
            }
        });
    }
}
