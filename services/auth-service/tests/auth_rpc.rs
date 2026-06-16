//! Split-mode proof: an [`Auth`] impl served over NATS round-trips through
//! [`AuthNatsClient`] exactly like a direct trait call — success responses, the
//! typed-error mapping (incl. the `InvalidArgument` detail + the `RateLimited`
//! retry-after number), and the `LoginOutcome` oneof. Uses a mock `Auth` so it
//! needs only live NATS (no Postgres). Skips cleanly if NATS is down.

#![allow(clippy::unwrap_used)]

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use auth_service::rpc::{AuthNatsClient, serve};
use auth_service::{Auth, AuthError, LoginOutcome, TotpEnrollment};
use dice_common::id::UserId;
use dice_event_bus::rpc::RpcClient;
use dice_protocol::v1;

/// Matches infrastructure/docker/docker-compose.yml + .env.example.
const DEV_NATS: &str = "nats://localhost:4222";

fn success(token: &str) -> v1::AuthSuccess {
    v1::AuthSuccess {
        access_token: token.to_owned(),
        refresh_token: "drt_x".to_owned(),
        access_expires_in_s: 600,
        user: Some(v1::User {
            id: 7,
            username: "u".to_owned(),
            display_name: String::new(),
            flags: 0,
            avatar_id: 0,
        }),
    }
}

/// A canned [`Auth`] hitting every RPC code path. Sentinel inputs trigger the
/// typed errors; everything else succeeds.
struct MockAuth;

#[async_trait::async_trait]
impl Auth for MockAuth {
    async fn register(
        &self,
        email: &str,
        _username: &str,
        _password: &str,
        _ip: Option<IpAddr>,
    ) -> Result<v1::AuthSuccess, AuthError> {
        match email {
            "taken@x" => Err(AuthError::EmailTaken),
            "bad@x" => Err(AuthError::InvalidArgument("weak password".to_owned())),
            _ => Ok(success("reg")),
        }
    }

    async fn login(
        &self,
        email: &str,
        password: &str,
        _ip: Option<IpAddr>,
    ) -> Result<LoginOutcome, AuthError> {
        if password == "bad" {
            return Err(AuthError::InvalidCredentials);
        }
        if email == "totp@x" {
            return Ok(LoginOutcome::TotpRequired {
                ticket: "tk".to_owned(),
            });
        }
        Ok(LoginOutcome::Success(Box::new(success("login"))))
    }

    async fn complete_totp_login(
        &self,
        _ticket: &str,
        code: &str,
    ) -> Result<v1::AuthSuccess, AuthError> {
        if code == "bad" {
            return Err(AuthError::InvalidTotp);
        }
        Ok(success("totp"))
    }

    async fn totp_enroll(&self, _user: UserId) -> Result<TotpEnrollment, AuthError> {
        Ok(TotpEnrollment {
            secret: "SEKRET".to_owned(),
            otpauth_uri: "otpauth://x".to_owned(),
        })
    }

    async fn totp_confirm(&self, _user: UserId, _code: &str) -> Result<Vec<String>, AuthError> {
        Ok(vec!["r1".to_owned(), "r2".to_owned()])
    }

    async fn totp_disable(&self, _user: UserId, _code: &str) -> Result<(), AuthError> {
        Ok(())
    }

    async fn verify_email(&self, _token: &str) -> Result<(), AuthError> {
        Ok(())
    }

    async fn resend_verification(&self, _user: UserId) -> Result<(), AuthError> {
        Ok(())
    }

    async fn request_password_reset(
        &self,
        email: &str,
        _ip: Option<IpAddr>,
    ) -> Result<(), AuthError> {
        if email == "rl@x" {
            return Err(AuthError::RateLimited {
                retry_after_ms: 5000,
            });
        }
        Ok(())
    }

    async fn reset_password(&self, _token: &str, _new_password: &str) -> Result<(), AuthError> {
        Ok(())
    }

    async fn refresh(&self, refresh_token: &str) -> Result<v1::AuthSuccess, AuthError> {
        if refresh_token == "bad" {
            return Err(AuthError::InvalidToken);
        }
        Ok(success("refresh"))
    }

    async fn logout(&self, _refresh_token: &str) -> Result<(), AuthError> {
        Ok(())
    }
}

#[tokio::test]
async fn auth_round_trips_over_nats() {
    let url = std::env::var("DICE_NATS_URL").unwrap_or_else(|_| DEV_NATS.to_owned());
    let Ok(server) = RpcClient::connect(&url).await else {
        eprintln!("skipping: live NATS required (just infra-up)");
        return;
    };
    let task = tokio::spawn(serve(server, Arc::new(MockAuth)));
    // Let the queue subscription register before the first request.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = AuthNatsClient::new(RpcClient::connect(&url).await.unwrap());

    // A success response decodes (nested User survives the round-trip).
    let s = client.register("a@x", "user", "pw", None).await.unwrap();
    assert_eq!(s.access_token, "reg");
    assert_eq!(s.user.unwrap().id, 7);

    // Code-only typed errors map back.
    assert!(matches!(
        client.register("taken@x", "u", "p", None).await,
        Err(AuthError::EmailTaken)
    ));
    assert!(matches!(
        client.login("a@x", "bad", None).await,
        Err(AuthError::InvalidCredentials)
    ));

    // InvalidArgument carries its detail string across the wire.
    match client.register("bad@x", "u", "p", None).await {
        Err(AuthError::InvalidArgument(m)) => assert_eq!(m, "weak password"),
        other => panic!("expected InvalidArgument, got {other:?}"),
    }

    // The LoginOutcome oneof round-trips both arms.
    assert!(matches!(
        client.login("a@x", "pw", None).await.unwrap(),
        LoginOutcome::Success(_)
    ));
    match client.login("totp@x", "pw", None).await.unwrap() {
        LoginOutcome::TotpRequired { ticket } => assert_eq!(ticket, "tk"),
        other => panic!("expected TotpRequired, got {other:?}"),
    }

    // Non-trivial responses decode.
    let enroll = client.totp_enroll(UserId::from_raw(7)).await.unwrap();
    assert_eq!(enroll.secret, "SEKRET");
    let codes = client
        .totp_confirm(UserId::from_raw(7), "123456")
        .await
        .unwrap();
    assert_eq!(codes, vec!["r1".to_owned(), "r2".to_owned()]);

    // RateLimited carries the retry-after number across the wire.
    match client.request_password_reset("rl@x", None).await {
        Err(AuthError::RateLimited { retry_after_ms }) => assert_eq!(retry_after_ms, 5000),
        other => panic!("expected RateLimited, got {other:?}"),
    }

    // A unit method + an IP that the server parses back.
    client.logout("drt").await.unwrap();
    let ip: IpAddr = "203.0.113.7".parse().unwrap();
    client.register("ip@x", "u", "p", Some(ip)).await.unwrap();

    task.abort();
}
