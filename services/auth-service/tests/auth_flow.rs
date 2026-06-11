//! Live-infra integration tests for [`AuthService`].
//!
//! These hit the real dev Postgres (`DATABASE_URL`, falling back to the
//! documented compose URL on :5433). They are robust to concurrent runs:
//! every test mints unique usernames/emails, uses its own in-memory cache
//! (isolated rate-limit windows) and its own Local bus, and deletes the users
//! it created (cascades wipe sessions + refresh tokens).

#![allow(clippy::unwrap_used)]

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use auth_service::{Auth, AuthError, AuthService};
use dice_auth_core::token::{JwtKeys, REFRESH_PREFIX, verify_access};
use dice_cache::CacheConfig;
use dice_common::id::{SnowflakeGenerator, UserId};
use dice_event_bus::{BusConfig, BusEvent, EventBus, Subject};
use dice_protocol::internal::v1::bus_event;
use sqlx::PgPool;

/// Matches infrastructure/docker/docker-compose.yml + .env.example.
const DEV_DB: &str = "postgres://dice:dice_dev@localhost:5433/dice";
const PASSWORD: &str = "correct horse battery staple";

struct Harness {
    svc: AuthService,
    jwt: Arc<JwtKeys>,
    bus: Arc<dyn EventBus>,
    pool: PgPool,
}

async fn harness() -> Harness {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEV_DB.to_owned());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("live Postgres required (just infra-up)");
    let cache = dice_cache::connect(CacheConfig::Memory).await.unwrap();
    let jwt = Arc::new(JwtKeys::generate_ephemeral());
    // ONE generator per process (like one per node in production): separate
    // node-0 generators in parallel tests would mint colliding snowflakes.
    static IDS: std::sync::OnceLock<Arc<SnowflakeGenerator>> = std::sync::OnceLock::new();
    let ids = Arc::clone(IDS.get_or_init(|| Arc::new(SnowflakeGenerator::new(0).unwrap())));
    let bus = dice_event_bus::connect(BusConfig::Local { capacity: 64 })
        .await
        .unwrap();
    let svc = AuthService::new(
        pool.clone(),
        cache,
        Arc::clone(&jwt),
        Arc::clone(&ids),
        Arc::clone(&bus),
    );
    Harness {
        svc,
        jwt,
        bus,
        pool,
    }
}

/// Unique username-safe suffix: pid + sub-second nanos + process counter —
/// collision-proof across parallel tests AND parallel test binaries.
fn unique(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        % 1_000_000_000_000;
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}{}x{nanos}x{c}", std::process::id())
}

fn email_for(username: &str) -> String {
    format!("{username}@test.dice")
}

async fn cleanup(pool: &PgPool, user_ids: &[u64]) {
    for id in user_ids {
        let _ = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(*id as i64)
            .execute(pool)
            .await;
    }
}

async fn expect_session_revoked(
    sub: &mut dice_event_bus::BusSubscription,
    user_id: u64,
) -> BusEvent {
    let event = tokio::time::timeout(Duration::from_secs(5), sub.recv())
        .await
        .expect("SessionRevoked must arrive within 5 s")
        .expect("bus must stay open");
    assert_eq!(event.origin, "auth-service");
    assert!(!event.ephemeral);
    assert_eq!(event.recipient_user_ids, vec![user_id]);
    assert!(event.event_id > 0);
    assert!(event.emitted_at_ms > 0);
    match &event.payload {
        Some(bus_event::Payload::SessionRevoked(sr)) => assert_eq!(sr.user_id, user_id),
        other => panic!("expected SessionRevoked payload, got {other:?}"),
    }
    event
}

#[tokio::test]
async fn full_lifecycle_register_login_refresh_reuse_detection_logout() {
    let h = harness().await;
    let username = unique("fl");
    let email = email_for(&username);

    // -- register --------------------------------------------------------
    let reg = h
        .svc
        .register(
            &email,
            &username,
            PASSWORD,
            Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9))),
        )
        .await
        .unwrap();
    assert!(reg.refresh_token.starts_with(REFRESH_PREFIX));
    assert_eq!(reg.access_expires_in_s, 600);
    let user = reg.user.clone().expect("register returns the user");
    assert_eq!(user.username, username);
    assert_eq!(
        user.display_name, username,
        "display_name starts as username"
    );
    let reg_claims = verify_access(&h.jwt, &reg.access_token).unwrap();
    assert_eq!(reg_claims.sub, user.id.to_string());

    // -- duplicate email / username -> typed errors -----------------------
    assert!(matches!(
        h.svc.register(&email, &unique("fl"), PASSWORD, None).await,
        Err(AuthError::EmailTaken)
    ));
    assert!(matches!(
        h.svc
            .register(&email_for(&unique("fl")), &username, PASSWORD, None)
            .await,
        Err(AuthError::UsernameTaken)
    ));

    // -- login -------------------------------------------------------------
    let login = h.svc.login(&email, PASSWORD, None).await.unwrap();
    let login_claims = verify_access(&h.jwt, &login.access_token).unwrap();
    assert_eq!(login_claims.sub, reg_claims.sub);
    assert_ne!(
        login_claims.sid, reg_claims.sid,
        "login mints a new auth_session"
    );
    assert_ne!(login.refresh_token, reg.refresh_token);

    // -- refresh: rotation within the SAME auth_session --------------------
    let refreshed = h.svc.refresh(&login.refresh_token).await.unwrap();
    assert_ne!(refreshed.refresh_token, login.refresh_token);
    let refreshed_claims = verify_access(&h.jwt, &refreshed.access_token).unwrap();
    assert_eq!(refreshed_claims.sid, login_claims.sid);
    assert_eq!(refreshed_claims.sub, login_claims.sub);
    assert_eq!(refreshed.user.as_ref().unwrap().username, username);

    // -- reuse of the rotated token: theft -> session revoked + bus event --
    let uid = UserId::from_raw(user.id);
    let mut sub = h.bus.subscribe(Subject::User(uid)).await.unwrap();
    assert!(matches!(
        h.svc.refresh(&login.refresh_token).await,
        Err(AuthError::InvalidToken)
    ));
    let event = expect_session_revoked(&mut sub, user.id).await;
    match event.payload {
        Some(bus_event::Payload::SessionRevoked(sr)) => {
            assert_eq!(
                sr.auth_session_id,
                login_claims.sid.parse::<u64>().unwrap(),
                "the LOGIN session (owner of the reused token) is revoked"
            );
        }
        _ => unreachable!("checked by expect_session_revoked"),
    }
    // The freshly rotated child died with the session.
    assert!(matches!(
        h.svc.refresh(&refreshed.refresh_token).await,
        Err(AuthError::InvalidToken)
    ));

    // -- logout: idempotent, publishes SessionRevoked ----------------------
    let mut sub2 = h.bus.subscribe(Subject::User(uid)).await.unwrap();
    h.svc.logout(&reg.refresh_token).await.unwrap();
    let event = expect_session_revoked(&mut sub2, user.id).await;
    match event.payload {
        Some(bus_event::Payload::SessionRevoked(sr)) => {
            assert_eq!(
                sr.auth_session_id,
                reg_claims.sid.parse::<u64>().unwrap(),
                "logout revokes the session owning the presented token"
            );
        }
        _ => unreachable!("checked by expect_session_revoked"),
    }
    h.svc.logout(&reg.refresh_token).await.unwrap(); // second logout: still Ok
    assert!(matches!(
        h.svc.refresh(&reg.refresh_token).await,
        Err(AuthError::InvalidToken)
    ));

    cleanup(&h.pool, &[user.id]).await;
}

#[tokio::test]
async fn login_rejects_wrong_password_and_unknown_email() {
    let h = harness().await;
    let username = unique("wp");
    let email = email_for(&username);
    let reg = h
        .svc
        .register(&email, &username, PASSWORD, None)
        .await
        .unwrap();

    assert!(matches!(
        h.svc.login(&email, "definitely-not-it", None).await,
        Err(AuthError::InvalidCredentials)
    ));
    // Unknown email: same error (and the dummy verify burns real CPU, so
    // there is no instant-return enumeration oracle — correctness only here).
    assert!(matches!(
        h.svc.login(&email_for(&unique("wp")), PASSWORD, None).await,
        Err(AuthError::InvalidCredentials)
    ));
    // Case-insensitive email still logs in.
    assert!(
        h.svc
            .login(&email.to_uppercase(), PASSWORD, None)
            .await
            .is_ok()
    );

    cleanup(&h.pool, &[reg.user.unwrap().id]).await;
}

#[tokio::test]
async fn refresh_and_logout_reject_garbage_tokens_fast() {
    let h = harness().await;
    let long = format!("drt_{}", "A".repeat(10));
    for garbage in ["", "drt_", "not-a-token", "drt_!!!not-base64!!!", &long] {
        assert!(
            matches!(h.svc.refresh(garbage).await, Err(AuthError::InvalidToken)),
            "refresh must reject {garbage:?}"
        );
        assert!(
            matches!(h.svc.logout(garbage).await, Err(AuthError::InvalidToken)),
            "logout must reject {garbage:?}"
        );
    }
    // Well-formed but unknown token: refresh fails; logout is idempotent Ok.
    let (token, _) = dice_auth_core::token::mint_refresh();
    assert!(matches!(
        h.svc.refresh(&token).await,
        Err(AuthError::InvalidToken)
    ));
    h.svc.logout(&token).await.unwrap();
}

#[tokio::test]
async fn register_validates_inputs_before_any_side_effect() {
    let h = harness().await;
    let ok_user = unique("va");

    for bad_email in ["", "plain", "@x.com", "a@", "a@nodot", "a b@x.com"] {
        assert!(
            matches!(
                h.svc.register(bad_email, &ok_user, PASSWORD, None).await,
                Err(AuthError::InvalidArgument(_))
            ),
            "email {bad_email:?} must be rejected"
        );
    }
    let too_long = "a".repeat(33);
    for bad_user in ["", "a", "UPPER", "has-dash", "has space", too_long.as_str()] {
        assert!(
            matches!(
                h.svc
                    .register(&email_for(&ok_user), bad_user, PASSWORD, None)
                    .await,
                Err(AuthError::InvalidArgument(_))
            ),
            "username {bad_user:?} must be rejected"
        );
    }
    for bad_password in ["1234567", &"p".repeat(129)] {
        assert!(matches!(
            h.svc
                .register(&email_for(&ok_user), &ok_user, bad_password, None)
                .await,
            Err(AuthError::InvalidArgument(_))
        ));
    }
    // Nothing was written (validation precedes DB + rate limit).
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE username = $1")
        .bind(&ok_user)
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn login_rate_limit_per_email_kicks_in_at_six() {
    let h = harness().await; // fresh in-memory cache => isolated windows
    let email = email_for(&unique("rl"));
    for attempt in 1..=5 {
        assert!(
            matches!(
                h.svc.login(&email, "whatever-pw", None).await,
                Err(AuthError::InvalidCredentials)
            ),
            "attempt {attempt} is under the 5/5min email limit"
        );
    }
    match h.svc.login(&email, "whatever-pw", None).await {
        Err(AuthError::RateLimited { retry_after_ms }) => assert!(retry_after_ms > 0),
        other => panic!("6th login must be rate limited, got {other:?}"),
    }
}

#[tokio::test]
async fn register_rate_limit_per_ip_kicks_in_at_four() {
    let h = harness().await;
    let ip = Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 77)));
    let mut created = Vec::new();
    for _ in 0..3 {
        let u = unique("rr");
        let ok = h
            .svc
            .register(&email_for(&u), &u, PASSWORD, ip)
            .await
            .unwrap();
        created.push(ok.user.unwrap().id);
    }
    let u = unique("rr");
    assert!(matches!(
        h.svc.register(&email_for(&u), &u, PASSWORD, ip).await,
        Err(AuthError::RateLimited { .. })
    ));

    // created_ip round-tripped through the ::inet cast.
    let stored: Option<String> =
        sqlx::query_scalar("SELECT host(created_ip) FROM auth_sessions WHERE user_id = $1")
            .bind(created[0] as i64)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(stored.as_deref(), Some("203.0.113.77"));

    cleanup(&h.pool, &created).await;
}
