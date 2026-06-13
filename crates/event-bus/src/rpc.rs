//! Minimal NATS request-reply RPC for split-mode service-to-service calls
//! (docs/design/backend-services.md §"Split mode"). The monolith calls services
//! by direct trait call; split deployments put a `*NatsClient` (this module's
//! [`RpcClient`]) behind the same `Arc<dyn Trait>` seam, and each service bin
//! runs a [`serve`] loop.
//!
//! - Subjects: `dice.rpc.{service}.{method}`.
//! - Servers `queue_subscribe` the `dice.rpc.{service}.*` wildcard under a queue
//!   group named for the service, so N replicas load-balance for free with no
//!   discovery (NATS picks one responder per request).
//! - Reply envelope is framed by hand — a 1-byte tag so no extra proto is
//!   needed: `0x00` + response bytes on success, `0x01` + u32-LE code +
//!   UTF-8 message on a domain fault. Method payloads are protobuf (rpc.proto).

use std::future::Future;
use std::sync::Arc;

use async_nats::Client;
use futures_util::StreamExt as _;

/// Root of every RPC subject.
pub const SUBJECT_ROOT: &str = "dice.rpc";
const TAG_OK: u8 = 0x00;
const TAG_ERR: u8 = 0x01;

/// A client-side RPC failure.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// NATS transport problem (no responder, timeout, connection).
    #[error("rpc transport: {0}")]
    Transport(String),
    /// The responder returned a domain fault (mapped back to a typed error by
    /// the calling stub via `code`).
    #[error("rpc fault {code}: {message}")]
    Fault { code: u32, message: String },
    /// The reply envelope was empty or malformed.
    #[error("malformed rpc reply")]
    MalformedReply,
}

/// A domain fault a responder sends back (the server-side counterpart of
/// [`RpcError::Fault`]). `code` is a service-defined discriminant the client
/// stub maps to its typed error; `0` is the catch-all "internal".
#[derive(Debug, Clone)]
pub struct RpcFault {
    pub code: u32,
    pub message: String,
}

impl RpcFault {
    /// Catch-all internal fault (code 0).
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: 0,
            message: message.into(),
        }
    }
}

/// Request side of the RPC seam: wraps one NATS [`Client`].
#[derive(Clone)]
pub struct RpcClient {
    client: Client,
}

impl RpcClient {
    /// Connect a dedicated NATS client for RPC.
    pub async fn connect(url: &str) -> Result<Self, RpcError> {
        let client = async_nats::connect(url)
            .await
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        Ok(Self { client })
    }

    /// Reuse an existing NATS client (e.g. the one the event bus already holds).
    #[must_use]
    pub fn from_client(client: Client) -> Self {
        Self { client }
    }

    /// Serve `dice.rpc.{service}.*` from this client's connection (see the free
    /// [`serve`]). Lets a service bin own one [`RpcClient`] for both calling and
    /// responding without depending on `async-nats` directly.
    pub async fn serve<H, Fut>(&self, service: &str, handler: H) -> Result<(), RpcError>
    where
        H: Fn(String, Vec<u8>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<u8>, RpcFault>> + Send + 'static,
    {
        serve(self.client.clone(), service, handler).await
    }

    /// Issue `dice.rpc.{service}.{method}` with `req`, await the reply, and
    /// decode the envelope into the raw response bytes or a typed error.
    pub async fn call(
        &self,
        service: &str,
        method: &str,
        req: Vec<u8>,
    ) -> Result<Vec<u8>, RpcError> {
        let subject = format!("{SUBJECT_ROOT}.{service}.{method}");
        let msg = self
            .client
            .request(subject, req.into())
            .await
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        decode_reply(&msg.payload)
    }
}

/// Serve `dice.rpc.{service}.*` under queue group `{service}` until the spawned
/// future is dropped/aborted (the subscription stream ends only on shutdown).
/// `handler(method, request_bytes)` returns the response bytes or an [`RpcFault`].
/// Each request is handled on its own task so a slow call can't head-of-line
/// block the responder.
pub async fn serve<H, Fut>(client: Client, service: &str, handler: H) -> Result<(), RpcError>
where
    H: Fn(String, Vec<u8>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Vec<u8>, RpcFault>> + Send + 'static,
{
    let subject = format!("{SUBJECT_ROOT}.{service}.*");
    let prefix_len = format!("{SUBJECT_ROOT}.{service}.").len();
    let mut sub = client
        .queue_subscribe(subject, service.to_owned())
        .await
        .map_err(|e| RpcError::Transport(e.to_string()))?;
    let handler = Arc::new(handler);
    while let Some(msg) = sub.next().await {
        // Fire-and-forget requests (no reply subject) are ignored.
        let Some(reply) = msg.reply.clone() else {
            continue;
        };
        let method = msg.subject.as_str()[prefix_len..].to_owned();
        let req = msg.payload.to_vec();
        let client = client.clone();
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            let payload = match handler(method, req).await {
                Ok(resp) => encode_ok(resp),
                Err(fault) => encode_err(&fault),
            };
            let _ = client.publish(reply, payload.into()).await;
        });
    }
    Ok(())
}

fn encode_ok(resp: Vec<u8>) -> Vec<u8> {
    let mut v = Vec::with_capacity(resp.len() + 1);
    v.push(TAG_OK);
    v.extend_from_slice(&resp);
    v
}

fn encode_err(fault: &RpcFault) -> Vec<u8> {
    let bytes = fault.message.as_bytes();
    let mut v = Vec::with_capacity(5 + bytes.len());
    v.push(TAG_ERR);
    v.extend_from_slice(&fault.code.to_le_bytes());
    v.extend_from_slice(bytes);
    v
}

fn decode_reply(payload: &[u8]) -> Result<Vec<u8>, RpcError> {
    match payload.split_first() {
        Some((&TAG_OK, rest)) => Ok(rest.to_vec()),
        Some((&TAG_ERR, rest)) if rest.len() >= 4 => {
            let code = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
            let message = String::from_utf8_lossy(&rest[4..]).into_owned();
            Err(RpcError::Fault { code, message })
        }
        _ => Err(RpcError::MalformedReply),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;

    /// Matches infrastructure/docker/docker-compose.yml + .env.example.
    const DEV_NATS: &str = "nats://localhost:4222";

    /// A per-run service name so parallel test processes never share a queue
    /// group (which would let one process answer another's request).
    fn unique_service() -> String {
        static C: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 1_000_000_000;
        format!(
            "test{}x{}x{}",
            std::process::id(),
            nanos,
            C.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[tokio::test]
    async fn rpc_round_trips_ok_and_fault() {
        let url = std::env::var("DICE_NATS_URL").unwrap_or_else(|_| DEV_NATS.to_owned());
        let Ok(server_client) = async_nats::connect(&url).await else {
            eprintln!("skipping: live NATS required (just infra-up)");
            return;
        };
        let service = unique_service();

        // Echo handler: "echo" returns the request; "boom" returns a fault.
        let svc = service.clone();
        let server = tokio::spawn(async move {
            let _ = serve(server_client, &svc, |method, req| async move {
                match method.as_str() {
                    "echo" => Ok(req),
                    "boom" => Err(RpcFault {
                        code: 7,
                        message: "kaboom".to_owned(),
                    }),
                    other => Err(RpcFault::internal(format!("no method {other}"))),
                }
            })
            .await;
        });

        let client = RpcClient::connect(&url).await.unwrap();
        // Give the queue subscription a moment to register.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let echoed = client
            .call(&service, "echo", b"hello rpc".to_vec())
            .await
            .unwrap();
        assert_eq!(echoed, b"hello rpc");

        match client.call(&service, "boom", Vec::new()).await {
            Err(RpcError::Fault { code, message }) => {
                assert_eq!(code, 7);
                assert_eq!(message, "kaboom");
            }
            other => panic!("expected a fault, got {other:?}"),
        }

        match client.call(&service, "nope", Vec::new()).await {
            Err(RpcError::Fault { code, .. }) => assert_eq!(code, 0),
            other => panic!("expected an internal fault, got {other:?}"),
        }

        server.abort();
    }
}
