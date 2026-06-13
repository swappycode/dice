//! Live test for the bus-agnostic core ([`handle_event`]): real Postgres +
//! an in-memory cache, NO NATS. The JetStream `run` loop is thin glue over
//! this and needs the full profile (NATS) to exercise end-to-end.

#![allow(clippy::unwrap_used)]

use std::sync::{Arc, OnceLock};

use dice_cache::{CacheConfig, UnreadStore, connect as cache_connect};
use dice_common::{ChannelId, SnowflakeGenerator, UserId};
use dice_protocol::internal::v1::{BusEvent, bus_event};
use dice_protocol::v1::{self, frame};
use notification_service::handle_event;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

const KIND_GUILD_TEXT: i16 = 1;

fn ids() -> Arc<SnowflakeGenerator> {
    static IDS: OnceLock<Arc<SnowflakeGenerator>> = OnceLock::new();
    IDS.get_or_init(|| {
        let node = (std::process::id() % 1024) as u16;
        Arc::new(SnowflakeGenerator::new(node).unwrap())
    })
    .clone()
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://dice:dice_dev@localhost:5433/dice".to_owned());
    PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("live Postgres required (just infra-up)")
}

async fn new_user(pool: &PgPool) -> UserId {
    let id = UserId::from(ids().generate());
    let name = format!("nt{}", id.raw());
    sqlx::query!(
        "INSERT INTO users (id, username, email, password_hash) VALUES ($1, $2, $3, 'test-hash')",
        id.as_i64(),
        name,
        format!("{name}@test.dice")
    )
    .execute(pool)
    .await
    .unwrap();
    id
}

fn msg_create_event(channel: ChannelId, author: UserId, content: &str) -> BusEvent {
    let frame = v1::Frame::dispatch(frame::Payload::MessageCreate(v1::MessageCreate {
        message: Some(v1::Message {
            id: ids().generate().0,
            channel_id: channel.raw(),
            author_id: author.raw(),
            content: content.to_owned(),
            edited_at_ms: 0,
            reply_to_id: 0,
            reactions: Vec::new(),
            attachments: Vec::new(),
        }),
        nonce: 0,
    }));
    BusEvent {
        event_id: ids().generate().0,
        emitted_at_ms: 1,
        origin: "test".to_owned(),
        guild_id: 0,
        recipient_user_ids: Vec::new(),
        ephemeral: false,
        payload: Some(bus_event::Payload::Frame(frame)),
    }
}

#[tokio::test]
async fn message_create_bumps_unread_for_members_except_author() {
    let pool = pool().await;
    let a = new_user(&pool).await;
    let b = new_user(&pool).await;

    // A guild owned by `a` with one channel; both users are members.
    let gid = ids().generate().as_i64();
    let chid = ids().generate().as_i64();
    let code = format!("ntfy{:04}", (gid as u64) % 10000);
    sqlx::query!(
        "INSERT INTO guilds (id, name, owner_id, invite_code) VALUES ($1, 'ntfy', $2, $3)",
        gid,
        a.as_i64(),
        code
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO channels (id, channel_type, guild_id, name, position) VALUES ($1, $2, $3, 'general', 0)",
        chid,
        KIND_GUILD_TEXT,
        gid
    )
    .execute(&pool)
    .await
    .unwrap();
    for u in [a, b] {
        sqlx::query!(
            "INSERT INTO guild_members (guild_id, user_id, permissions) VALUES ($1, $2, 0)",
            gid,
            u.as_i64()
        )
        .execute(&pool)
        .await
        .unwrap();
    }

    let unread = UnreadStore::new(cache_connect(CacheConfig::Memory).await.unwrap());
    let channel = ChannelId::from_i64(chid);

    handle_event(&pool, &unread, &msg_create_event(channel, a, "hi"))
        .await
        .unwrap();
    assert_eq!(
        unread.count(b, channel).await.unwrap(),
        1,
        "member notified"
    );
    assert_eq!(
        unread.count(a, channel).await.unwrap(),
        0,
        "author is never notified of their own message"
    );

    handle_event(&pool, &unread, &msg_create_event(channel, a, "again"))
        .await
        .unwrap();
    assert_eq!(unread.count(b, channel).await.unwrap(), 2, "accumulates");

    // A non-MessageCreate event is a no-op.
    handle_event(&pool, &unread, &BusEvent::default())
        .await
        .unwrap();
    assert_eq!(unread.count(b, channel).await.unwrap(), 2);

    // cleanup (guild cascades channels + members; then the users).
    let _ = sqlx::query!("DELETE FROM guilds WHERE id = $1", gid)
        .execute(&pool)
        .await;
    let _ = sqlx::query!(
        "DELETE FROM users WHERE id = ANY($1::bigint[])",
        &[a.as_i64(), b.as_i64()][..]
    )
    .execute(&pool)
    .await;
}
