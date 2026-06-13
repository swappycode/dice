//! Live-infra integration tests: real Postgres (DATABASE_URL, default
//! `postgres://dice:dice_dev@localhost:5433/dice`) + the in-process Local bus.
//!
//! Robust to concurrent runs: every test mints fresh users via a
//! process-shared snowflake generator with a random node id, usernames embed
//! the user id, and each test deletes its own rows at the end (best-effort).

#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chat_service::{Chat, ChatError, ChatService, HistoryCursor};
use dice_common::{ChannelId, GuildId, MessageId, SnowflakeGenerator, UserId};
use dice_event_bus::{BusConfig, BusEvent, BusSubscription, EventBus, Subject};
use dice_permissions::{DEFAULT_EVERYONE, Permissions};
use dice_protocol::internal::v1::bus_event;
use dice_protocol::v1::{self, frame};
use rand::Rng;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::time::timeout;

/// One generator per test PROCESS (unique ids within the process by
/// construction; the random node id makes cross-process collisions with
/// concurrent sibling runs a ~1/1024-per-millisecond long shot).
fn ids() -> Arc<SnowflakeGenerator> {
    static IDS: OnceLock<Arc<SnowflakeGenerator>> = OnceLock::new();
    IDS.get_or_init(|| {
        let node = rand::rng().random_range(0..1024u16);
        Arc::new(SnowflakeGenerator::new(node).unwrap())
    })
    .clone()
}

struct Ctx {
    pool: PgPool,
    bus: Arc<dyn EventBus>,
    svc: ChatService,
    users: Vec<UserId>,
}

impl Ctx {
    async fn new(n_users: usize) -> Self {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://dice:dice_dev@localhost:5433/dice".to_owned());
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .expect("live Postgres required (just infra-up)");
        let bus = dice_event_bus::connect(BusConfig::default()).await.unwrap();
        let svc = ChatService::new(pool.clone(), bus.clone(), ids());
        let mut users = Vec::with_capacity(n_users);
        for _ in 0..n_users {
            users.push(new_user(&pool).await);
        }
        Self {
            pool,
            bus,
            svc,
            users,
        }
    }

    /// Best-effort cleanup: guilds owned by test users cascade their members,
    /// channels, and messages; DM channels cascade via the recipients
    /// subquery; users go last (messages reference authors without CASCADE).
    async fn finish(self) {
        let raw: Vec<i64> = self.users.iter().map(|u| u.as_i64()).collect();
        let _ = sqlx::query!(
            "DELETE FROM guilds WHERE owner_id = ANY($1::bigint[])",
            &raw[..]
        )
        .execute(&self.pool)
        .await;
        let _ = sqlx::query!(
            "DELETE FROM channels WHERE id IN \
             (SELECT channel_id FROM channel_recipients WHERE user_id = ANY($1::bigint[]))",
            &raw[..]
        )
        .execute(&self.pool)
        .await;
        let _ = sqlx::query!("DELETE FROM users WHERE id = ANY($1::bigint[])", &raw[..])
            .execute(&self.pool)
            .await;
    }
}

async fn new_user(pool: &PgPool) -> UserId {
    let id = UserId::from(ids().generate());
    let name = format!("ct{}", id.raw());
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

async fn next_event(sub: &mut BusSubscription) -> BusEvent {
    timeout(Duration::from_secs(5), sub.recv())
        .await
        .expect("timed out waiting for bus event")
        .expect("bus closed")
}

async fn expect_silence(sub: &mut BusSubscription) {
    let res = timeout(Duration::from_millis(300), sub.recv()).await;
    assert!(res.is_err(), "unexpected bus event: {:?}", res.unwrap());
}

/// Unwrap the BusEvent down to the dispatch frame payload, asserting the bus
/// contract: seq=0 (the gateway assigns per-session seq) and origin.
fn dispatch(ev: &BusEvent) -> &frame::Payload {
    assert_eq!(ev.origin, "chat-service");
    assert!(ev.event_id > 0, "event_id must be a real snowflake");
    assert!(ev.emitted_at_ms > 0);
    let Some(bus_event::Payload::Frame(f)) = &ev.payload else {
        panic!("expected a Frame payload: {ev:?}");
    };
    assert_eq!(f.seq, 0, "bus frames must carry seq=0");
    f.payload.as_ref().expect("frame payload missing")
}

fn sorted(mut v: Vec<u64>) -> Vec<u64> {
    v.sort_unstable();
    v
}

#[tokio::test]
async fn create_guild_creates_general_and_publishes_guild_create() {
    let ctx = Ctx::new(1).await;
    let owner = ctx.users[0];
    let mut user_sub = ctx.bus.subscribe(Subject::User(owner)).await.unwrap();

    let guild = ctx
        .svc
        .create_guild(owner, "  Test Guild  ".to_owned())
        .await
        .unwrap();
    assert_eq!(guild.name, "Test Guild");
    assert_eq!(guild.owner_id, owner.raw());
    assert_eq!(guild.invite_code.len(), 8);
    assert!(
        guild
            .invite_code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
        "invite code charset: {:?}",
        guild.invite_code
    );
    assert_eq!(guild.channels.len(), 1, "exactly #general");
    let general = &guild.channels[0];
    assert_eq!(general.name, "general");
    assert_eq!(general.kind, v1::ChannelKind::GuildText as i32);
    assert_eq!(general.position, 0);
    assert_eq!(general.guild_id, guild.id);
    assert_eq!(guild.members.len(), 1, "owner is a member");
    assert_eq!(guild.members[0].user_id, owner.raw());
    assert_eq!(guild.members[0].permissions, DEFAULT_EVERYONE.bits());

    let ev = next_event(&mut user_sub).await;
    assert!(!ev.ephemeral);
    assert_eq!(ev.guild_id, guild.id);
    let frame::Payload::GuildCreate(gc) = dispatch(&ev) else {
        panic!("expected GuildCreate, got {ev:?}");
    };
    assert_eq!(gc.guild.as_ref().unwrap(), &guild, "full guild on the bus");

    ctx.finish().await;
}

#[tokio::test]
async fn create_guild_rejects_invalid_names() {
    let ctx = Ctx::new(1).await;
    let u = ctx.users[0];
    let too_long = "x".repeat(101);
    for bad in ["", "   \t ", too_long.as_str()] {
        let err = ctx.svc.create_guild(u, bad.to_owned()).await.unwrap_err();
        assert!(
            matches!(err, ChatError::InvalidArgument(_)),
            "{bad:?} -> {err:?}"
        );
    }
    // Exactly 100 chars is allowed.
    let g = ctx.svc.create_guild(u, "x".repeat(100)).await.unwrap();
    assert_eq!(g.name.chars().count(), 100);
    ctx.finish().await;
}

#[tokio::test]
async fn join_guild_publishes_member_add_and_is_idempotent() {
    let ctx = Ctx::new(2).await;
    let (a, b) = (ctx.users[0], ctx.users[1]);
    let guild = ctx.svc.create_guild(a, "join-test".into()).await.unwrap();
    let gid = GuildId::from_raw(guild.id);

    let mut guild_sub = ctx.bus.subscribe(Subject::GuildMsg(gid)).await.unwrap();
    let mut user_sub = ctx.bus.subscribe(Subject::User(b)).await.unwrap();

    // Generated codes are always [a-z0-9], so this can never exist.
    let err = ctx.svc.join_guild(b, "!nv@lid!").await.unwrap_err();
    assert!(matches!(err, ChatError::InvalidInvite), "{err:?}");

    let joined = ctx.svc.join_guild(b, &guild.invite_code).await.unwrap();
    assert_eq!(joined.id, guild.id);
    assert_eq!(joined.members.len(), 2);
    assert!(joined.members.iter().any(|m| m.user_id == b.raw()));

    let ev = next_event(&mut guild_sub).await;
    assert!(!ev.ephemeral);
    assert_eq!(ev.guild_id, guild.id);
    let frame::Payload::MemberAdd(ma) = dispatch(&ev) else {
        panic!("expected GuildMemberAdd, got {ev:?}");
    };
    let member = ma.member.as_ref().unwrap();
    assert_eq!(member.user_id, b.raw());
    assert_eq!(member.guild_id, guild.id);
    assert_eq!(member.permissions, DEFAULT_EVERYONE.bits());
    assert!(member.joined_at_ms > 0);
    let user = ma.user.as_ref().unwrap();
    assert_eq!(user.id, b.raw());
    assert_eq!(user.username, format!("ct{}", b.raw()));

    let ev = next_event(&mut user_sub).await;
    let frame::Payload::GuildCreate(gc) = dispatch(&ev) else {
        panic!("expected GuildCreate on the joiner's user subject, got {ev:?}");
    };
    let g = gc.guild.as_ref().unwrap();
    assert_eq!(g.id, guild.id);
    assert_eq!(g.members.len(), 2, "full guild including the joiner");
    assert_eq!(g.invite_code, guild.invite_code);

    // Idempotent re-join: same guild, no duplicate member, no new events.
    let again = ctx.svc.join_guild(b, &guild.invite_code).await.unwrap();
    assert_eq!(again.id, guild.id);
    assert_eq!(again.members.len(), 2);
    expect_silence(&mut guild_sub).await;
    expect_silence(&mut user_sub).await;

    ctx.finish().await;
}

#[tokio::test]
async fn send_message_dispatches_with_nonce_and_updates_last_message_id() {
    let ctx = Ctx::new(1).await;
    let a = ctx.users[0];
    let guild = ctx.svc.create_guild(a, "msg-test".into()).await.unwrap();
    let gid = GuildId::from_raw(guild.id);
    let general = ChannelId::from_raw(guild.channels[0].id);

    let mut sub = ctx.bus.subscribe(Subject::GuildMsg(gid)).await.unwrap();
    let msg = ctx
        .svc
        .send_message(a, general, "  hello dice  ".into(), None, 42)
        .await
        .unwrap();
    assert_eq!(msg.content, "hello dice", "content is trimmed");
    assert_eq!(msg.channel_id, general.raw());
    assert_eq!(msg.author_id, a.raw());
    assert_eq!(msg.edited_at_ms, 0);
    assert!(msg.id > 0);

    let ev = next_event(&mut sub).await;
    assert!(!ev.ephemeral);
    assert_eq!(ev.guild_id, guild.id);
    let frame::Payload::MessageCreate(mc) = dispatch(&ev) else {
        panic!("expected MessageCreate, got {ev:?}");
    };
    assert_eq!(mc.nonce, 42, "dispatch carries the author's nonce");
    assert_eq!(mc.message.as_ref().unwrap(), &msg);

    let last = sqlx::query_scalar!(
        "SELECT last_message_id FROM channels WHERE id = $1",
        general.as_i64()
    )
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(last, Some(msg.id as i64), "last_message_id updated in tx");

    ctx.finish().await;
}

#[tokio::test]
async fn send_message_requires_membership_and_send_permission() {
    let ctx = Ctx::new(2).await;
    let (a, b) = (ctx.users[0], ctx.users[1]);
    let guild = ctx.svc.create_guild(a, "perm-test".into()).await.unwrap();
    let general = ChannelId::from_raw(guild.channels[0].id);

    // Non-member.
    let err = ctx
        .svc
        .send_message(b, general, "hi".into(), None, 1)
        .await
        .unwrap_err();
    assert!(matches!(err, ChatError::NotAMember), "{err:?}");

    // Unknown channel (snowflakes are never 1).
    let err = ctx
        .svc
        .send_message(a, ChannelId::from_raw(1), "hi".into(), None, 1)
        .await
        .unwrap_err();
    assert!(matches!(err, ChatError::NotFound), "{err:?}");

    // Member stripped of SEND_MESSAGES.
    ctx.svc.join_guild(b, &guild.invite_code).await.unwrap();
    sqlx::query!(
        "UPDATE guild_members SET permissions = $1 WHERE guild_id = $2 AND user_id = $3",
        Permissions::VIEW_CHANNEL.to_db(),
        guild.id as i64,
        b.as_i64()
    )
    .execute(&ctx.pool)
    .await
    .unwrap();
    match ctx
        .svc
        .send_message(b, general, "hi".into(), None, 1)
        .await
        .unwrap_err()
    {
        ChatError::PermissionDenied(mp) => {
            assert!(mp.missing.contains(Permissions::SEND_MESSAGES), "{mp:?}");
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }

    // The owner always passes (compute() owner override).
    ctx.svc
        .send_message(a, general, "owner ok".into(), None, 2)
        .await
        .unwrap();

    ctx.finish().await;
}

#[tokio::test]
async fn edit_message_is_author_only_and_publishes_update() {
    let ctx = Ctx::new(2).await;
    let (a, b) = (ctx.users[0], ctx.users[1]);
    let guild = ctx.svc.create_guild(a, "edit-test".into()).await.unwrap();
    let gid = GuildId::from_raw(guild.id);
    let ch = ChannelId::from_raw(guild.channels[0].id);
    ctx.svc.join_guild(b, &guild.invite_code).await.unwrap();
    let msg = ctx
        .svc
        .send_message(a, ch, "typo heer".into(), None, 1)
        .await
        .unwrap();

    let mut sub = ctx.bus.subscribe(Subject::GuildMsg(gid)).await.unwrap();
    let edited = ctx
        .svc
        .edit_message(a, ch, MessageId::from_raw(msg.id), "  fixed here  ".into())
        .await
        .unwrap();
    assert_eq!(edited.id, msg.id);
    assert_eq!(edited.content, "fixed here", "trimmed");
    assert!(edited.edited_at_ms > 0, "edited_at stamped");

    let ev = next_event(&mut sub).await;
    assert!(!ev.ephemeral);
    let frame::Payload::MessageUpdate(mu) = dispatch(&ev) else {
        panic!("expected MessageUpdate, got {ev:?}");
    };
    assert_eq!(mu.message.as_ref().unwrap(), &edited);

    // Another member (even editing is never allowed for non-authors).
    let err = ctx
        .svc
        .edit_message(b, ch, MessageId::from_raw(msg.id), "hijack".into())
        .await
        .unwrap_err();
    assert!(matches!(err, ChatError::Forbidden(_)), "{err:?}");

    // Empty content is rejected.
    let err = ctx
        .svc
        .edit_message(a, ch, MessageId::from_raw(msg.id), "   ".into())
        .await
        .unwrap_err();
    assert!(matches!(err, ChatError::InvalidArgument(_)), "{err:?}");

    // Unknown message id.
    let err = ctx
        .svc
        .edit_message(a, ch, MessageId::from_raw(1), "x".into())
        .await
        .unwrap_err();
    assert!(matches!(err, ChatError::NotFound), "{err:?}");

    ctx.finish().await;
}

#[tokio::test]
async fn delete_message_allows_author_or_manage_messages() {
    let ctx = Ctx::new(2).await;
    let (a, b) = (ctx.users[0], ctx.users[1]);
    let guild = ctx.svc.create_guild(a, "del-test".into()).await.unwrap();
    let gid = GuildId::from_raw(guild.id);
    let ch = ChannelId::from_raw(guild.channels[0].id);
    ctx.svc.join_guild(b, &guild.invite_code).await.unwrap();

    // A plain member cannot delete someone else's message.
    let m1 = ctx
        .svc
        .send_message(a, ch, "a-one".into(), None, 1)
        .await
        .unwrap();
    let err = ctx
        .svc
        .delete_message(b, ch, MessageId::from_raw(m1.id))
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatError::PermissionDenied(mp) if mp.missing.contains(Permissions::MANAGE_MESSAGES)),
        "{err:?}"
    );

    // The author deletes their own; MessageDelete fans out with the ids.
    let mut sub = ctx.bus.subscribe(Subject::GuildMsg(gid)).await.unwrap();
    ctx.svc
        .delete_message(a, ch, MessageId::from_raw(m1.id))
        .await
        .unwrap();
    let ev = next_event(&mut sub).await;
    let frame::Payload::MessageDelete(md) = dispatch(&ev) else {
        panic!("expected MessageDelete, got {ev:?}");
    };
    assert_eq!(md.message_id, m1.id);
    assert_eq!(md.channel_id, ch.raw());
    let count = sqlx::query_scalar!("SELECT COUNT(*) FROM messages WHERE id = $1", m1.id as i64)
        .fetch_one(&ctx.pool)
        .await
        .unwrap();
    assert_eq!(count, Some(0), "row gone");

    // A moderator with MANAGE_MESSAGES deletes another user's message.
    let m2 = ctx
        .svc
        .send_message(a, ch, "a-two".into(), None, 2)
        .await
        .unwrap();
    sqlx::query!(
        "UPDATE guild_members SET permissions = $1 WHERE guild_id = $2 AND user_id = $3",
        (Permissions::VIEW_CHANNEL | Permissions::MANAGE_MESSAGES).to_db(),
        guild.id as i64,
        b.as_i64()
    )
    .execute(&ctx.pool)
    .await
    .unwrap();
    ctx.svc
        .delete_message(b, ch, MessageId::from_raw(m2.id))
        .await
        .unwrap();

    ctx.finish().await;
}

#[tokio::test]
async fn reply_to_id_round_trips_through_history() {
    let ctx = Ctx::new(1).await;
    let a = ctx.users[0];
    let guild = ctx.svc.create_guild(a, "reply-test".into()).await.unwrap();
    let ch = ChannelId::from_raw(guild.channels[0].id);
    let parent = ctx
        .svc
        .send_message(a, ch, "original".into(), None, 1)
        .await
        .unwrap();
    let reply = ctx
        .svc
        .send_message(
            a,
            ch,
            "a reply".into(),
            Some(MessageId::from_raw(parent.id)),
            2,
        )
        .await
        .unwrap();
    assert_eq!(reply.reply_to_id, parent.id);

    let page = ctx
        .svc
        .get_messages(a, ch, HistoryCursor::Latest, 10)
        .await
        .unwrap();
    assert_eq!(
        page.iter().find(|m| m.id == reply.id).unwrap().reply_to_id,
        parent.id,
        "history preserves reply_to_id"
    );
    assert_eq!(
        page.iter().find(|m| m.id == parent.id).unwrap().reply_to_id,
        0,
        "a non-reply is 0"
    );
    ctx.finish().await;
}

#[tokio::test]
async fn reactions_aggregate_in_history_and_broadcast_deltas() {
    let ctx = Ctx::new(2).await;
    let (a, b) = (ctx.users[0], ctx.users[1]);
    let guild = ctx.svc.create_guild(a, "react-test".into()).await.unwrap();
    let gid = GuildId::from_raw(guild.id);
    let ch = ChannelId::from_raw(guild.channels[0].id);
    ctx.svc.join_guild(b, &guild.invite_code).await.unwrap();
    let msg = ctx
        .svc
        .send_message(a, ch, "react to me".into(), None, 1)
        .await
        .unwrap();
    let mid = MessageId::from_raw(msg.id);

    let mut sub = ctx.bus.subscribe(Subject::GuildMsg(gid)).await.unwrap();
    ctx.svc.add_reaction(a, ch, mid, "👍".into()).await.unwrap();
    let ev = next_event(&mut sub).await;
    let frame::Payload::ReactionUpdate(ru) = dispatch(&ev) else {
        panic!("expected ReactionUpdate, got {ev:?}");
    };
    assert_eq!(
        (ru.message_id, ru.emoji.as_str(), ru.user_id, ru.added),
        (msg.id, "👍", a.raw(), true)
    );

    // Idempotent re-add fans out nothing.
    ctx.svc.add_reaction(a, ch, mid, "👍".into()).await.unwrap();
    expect_silence(&mut sub).await;

    ctx.svc.add_reaction(b, ch, mid, "👍".into()).await.unwrap();
    let _ = next_event(&mut sub).await;

    // History aggregate from a's perspective: 👍 ×2, me = true.
    let page = ctx
        .svc
        .get_messages(a, ch, HistoryCursor::Latest, 10)
        .await
        .unwrap();
    let got = page.iter().find(|m| m.id == msg.id).unwrap();
    assert_eq!(got.reactions.len(), 1);
    assert_eq!(
        (
            got.reactions[0].emoji.as_str(),
            got.reactions[0].count,
            got.reactions[0].me
        ),
        ("👍", 2, true)
    );

    ctx.svc
        .remove_reaction(a, ch, mid, "👍".into())
        .await
        .unwrap();
    let ev = next_event(&mut sub).await;
    let frame::Payload::ReactionUpdate(ru) = dispatch(&ev) else {
        panic!("expected ReactionUpdate");
    };
    assert!(!ru.added);
    let page2 = ctx
        .svc
        .get_messages(a, ch, HistoryCursor::Latest, 10)
        .await
        .unwrap();
    let got2 = page2.iter().find(|m| m.id == msg.id).unwrap();
    assert_eq!(
        (got2.reactions[0].count, got2.reactions[0].me),
        (1, false),
        "a's reaction removed"
    );

    // Reacting to an unknown message is NotFound.
    let err = ctx
        .svc
        .add_reaction(a, ch, MessageId::from_raw(1), "x".into())
        .await
        .unwrap_err();
    assert!(matches!(err, ChatError::NotFound), "{err:?}");

    ctx.finish().await;
}

#[tokio::test]
async fn get_messages_keyset_pagination_newest_first() {
    let ctx = Ctx::new(2).await;
    let (a, b) = (ctx.users[0], ctx.users[1]);
    let guild = ctx.svc.create_guild(a, "page-test".into()).await.unwrap();
    let ch = ChannelId::from_raw(guild.channels[0].id);

    let mut sent = Vec::new();
    for i in 0..15u64 {
        sent.push(
            ctx.svc
                .send_message(a, ch, format!("m{i}"), None, i)
                .await
                .unwrap(),
        );
    }

    // Page 1: Latest, newest-first.
    let p1 = ctx
        .svc
        .get_messages(a, ch, HistoryCursor::Latest, 10)
        .await
        .unwrap();
    assert_eq!(p1.len(), 10);
    assert!(p1.windows(2).all(|w| w[0].id > w[1].id), "newest-first");
    assert_eq!(p1[0].id, sent[14].id);
    assert_eq!(p1[9].id, sent[5].id);

    // Page 2: Before the oldest of page 1 — the remaining 5, no overlap.
    let p2 = ctx
        .svc
        .get_messages(
            a,
            ch,
            HistoryCursor::Before(MessageId::from_raw(p1[9].id)),
            10,
        )
        .await
        .unwrap();
    assert_eq!(p2.len(), 5);
    assert!(p2.windows(2).all(|w| w[0].id > w[1].id), "newest-first");
    assert_eq!(p2[0].id, sent[4].id);
    assert_eq!(p2[4].id, sent[0].id);
    let ids1: HashSet<u64> = p1.iter().map(|m| m.id).collect();
    assert!(p2.iter().all(|m| !ids1.contains(&m.id)), "no overlap");

    // After: gap backfill, still returned NEWEST-FIRST.
    let after = ctx
        .svc
        .get_messages(
            a,
            ch,
            HistoryCursor::After(MessageId::from_raw(sent[9].id)),
            100,
        )
        .await
        .unwrap();
    assert_eq!(after.len(), 5);
    assert!(after.windows(2).all(|w| w[0].id > w[1].id), "newest-first");
    assert_eq!(after[0].id, sent[14].id);
    assert_eq!(after[4].id, sent[10].id);

    // Limit clamps: 0 -> 1.
    let one = ctx
        .svc
        .get_messages(a, ch, HistoryCursor::Latest, 0)
        .await
        .unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].id, sent[14].id);

    // Non-members cannot read history.
    let err = ctx
        .svc
        .get_messages(b, ch, HistoryCursor::Latest, 10)
        .await
        .unwrap_err();
    assert!(matches!(err, ChatError::NotAMember), "{err:?}");

    ctx.finish().await;
}

#[tokio::test]
async fn open_dm_is_idempotent_and_notifies_both_users() {
    let ctx = Ctx::new(3).await;
    let (a, b, c) = (ctx.users[0], ctx.users[1], ctx.users[2]);
    let mut sub_a = ctx.bus.subscribe(Subject::User(a)).await.unwrap();
    let mut sub_b = ctx.bus.subscribe(Subject::User(b)).await.unwrap();

    let err = ctx.svc.open_dm(a, a).await.unwrap_err();
    assert!(matches!(err, ChatError::InvalidArgument(_)), "{err:?}");
    let err = ctx.svc.open_dm(a, UserId::from_raw(1)).await.unwrap_err();
    assert!(matches!(err, ChatError::NotFound), "{err:?}");

    let dm = ctx.svc.open_dm(a, b).await.unwrap();
    assert_eq!(dm.kind, v1::ChannelKind::Dm as i32);
    assert_eq!(dm.guild_id, 0);
    assert_eq!(dm.name, "");
    let want = sorted(vec![a.raw(), b.raw()]);
    assert_eq!(dm.recipient_ids, want);

    // DmChannelCreate lands on BOTH user subjects.
    for sub in [&mut sub_a, &mut sub_b] {
        let ev = next_event(sub).await;
        assert!(!ev.ephemeral);
        assert_eq!(ev.guild_id, 0);
        assert_eq!(sorted(ev.recipient_user_ids.clone()), want);
        let frame::Payload::DmChannelCreate(dc) = dispatch(&ev) else {
            panic!("expected DmChannelCreate, got {ev:?}");
        };
        assert_eq!(dc.channel.as_ref().unwrap().id, dm.id);
    }

    // Idempotent: same direction, then reversed — same channel, no events.
    let again = ctx.svc.open_dm(a, b).await.unwrap();
    assert_eq!(again.id, dm.id);
    let reversed = ctx.svc.open_dm(b, a).await.unwrap();
    assert_eq!(reversed.id, dm.id);
    assert_eq!(reversed.recipient_ids, want);
    expect_silence(&mut sub_a).await;
    expect_silence(&mut sub_b).await;

    // DM sends: non-recipient rejected; dispatch on the dm.msg subject with
    // recipient routing hints and the author's nonce.
    let dch = ChannelId::from_raw(dm.id);
    let err = ctx
        .svc
        .send_message(c, dch, "intrude".into(), None, 1)
        .await
        .unwrap_err();
    assert!(matches!(err, ChatError::NotAMember), "{err:?}");

    let mut dm_sub = ctx.bus.subscribe(Subject::DmMsg(dch)).await.unwrap();
    let msg = ctx
        .svc
        .send_message(a, dch, "dm hello".into(), None, 9)
        .await
        .unwrap();
    let ev = next_event(&mut dm_sub).await;
    assert_eq!(ev.guild_id, 0, "DMs are not guild-scoped");
    assert_eq!(sorted(ev.recipient_user_ids.clone()), want);
    let frame::Payload::MessageCreate(mc) = dispatch(&ev) else {
        panic!("expected MessageCreate, got {ev:?}");
    };
    assert_eq!(mc.nonce, 9);
    assert_eq!(mc.message.as_ref().unwrap().id, msg.id);

    ctx.finish().await;
}

#[tokio::test]
async fn create_channel_requires_manage_channels() {
    let ctx = Ctx::new(3).await;
    let (a, b, c) = (ctx.users[0], ctx.users[1], ctx.users[2]);
    let guild = ctx.svc.create_guild(a, "chan-test".into()).await.unwrap();
    let gid = GuildId::from_raw(guild.id);
    ctx.svc.join_guild(b, &guild.invite_code).await.unwrap();

    // Plain member: DEFAULT_EVERYONE lacks MANAGE_CHANNELS.
    match ctx
        .svc
        .create_channel(b, gid, "nope".into())
        .await
        .unwrap_err()
    {
        ChatError::PermissionDenied(mp) => {
            assert!(mp.missing.contains(Permissions::MANAGE_CHANNELS), "{mp:?}");
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
    // Non-member and unknown guild.
    let err = ctx
        .svc
        .create_channel(c, gid, "nope".into())
        .await
        .unwrap_err();
    assert!(matches!(err, ChatError::NotAMember), "{err:?}");
    let err = ctx
        .svc
        .create_channel(a, GuildId::from_raw(1), "x".into())
        .await
        .unwrap_err();
    assert!(matches!(err, ChatError::NotFound), "{err:?}");

    // Owner passes; position = max+1; ChannelCreate on the guild subject.
    let mut sub = ctx.bus.subscribe(Subject::GuildMsg(gid)).await.unwrap();
    let ch = ctx
        .svc
        .create_channel(a, gid, "  announcements ".into())
        .await
        .unwrap();
    assert_eq!(ch.name, "announcements");
    assert_eq!(ch.position, 1, "after #general at 0");
    assert_eq!(ch.kind, v1::ChannelKind::GuildText as i32);
    assert_eq!(ch.guild_id, guild.id);

    let ev = next_event(&mut sub).await;
    assert_eq!(ev.guild_id, guild.id);
    let frame::Payload::ChannelCreate(cc) = dispatch(&ev) else {
        panic!("expected ChannelCreate, got {ev:?}");
    };
    assert_eq!(cc.channel.as_ref().unwrap(), &ch);

    ctx.finish().await;
}

#[tokio::test]
async fn typing_publishes_ephemeral_to_the_right_subject() {
    let ctx = Ctx::new(2).await;
    let (a, b) = (ctx.users[0], ctx.users[1]);
    let guild = ctx.svc.create_guild(a, "typing-test".into()).await.unwrap();
    let gid = GuildId::from_raw(guild.id);
    let general = ChannelId::from_raw(guild.channels[0].id);

    let mut t_sub = ctx.bus.subscribe(Subject::GuildTyping(gid)).await.unwrap();
    ctx.svc.typing(a, general).await.unwrap();
    let ev = next_event(&mut t_sub).await;
    assert!(ev.ephemeral, "typing must never enter replay buffers");
    assert_eq!(ev.guild_id, guild.id);
    let frame::Payload::TypingStart(ts) = dispatch(&ev) else {
        panic!("expected TypingStart, got {ev:?}");
    };
    assert_eq!(ts.channel_id, general.raw());
    assert_eq!(ts.user_id, a.raw());

    // Membership is still required.
    let err = ctx.svc.typing(b, general).await.unwrap_err();
    assert!(matches!(err, ChatError::NotAMember), "{err:?}");

    // DM typing goes to dm.{channel}.typing.
    let dm = ctx.svc.open_dm(a, b).await.unwrap();
    let dch = ChannelId::from_raw(dm.id);
    let mut d_sub = ctx.bus.subscribe(Subject::DmTyping(dch)).await.unwrap();
    ctx.svc.typing(b, dch).await.unwrap();
    let ev = next_event(&mut d_sub).await;
    assert!(ev.ephemeral);
    assert_eq!(ev.guild_id, 0);
    let frame::Payload::TypingStart(ts) = dispatch(&ev) else {
        panic!("expected TypingStart, got {ev:?}");
    };
    assert_eq!(ts.channel_id, dm.id);
    assert_eq!(ts.user_id, b.raw());

    ctx.finish().await;
}

#[tokio::test]
async fn message_content_length_is_validated() {
    let ctx = Ctx::new(1).await;
    let a = ctx.users[0];
    let guild = ctx.svc.create_guild(a, "len-test".into()).await.unwrap();
    let ch = ChannelId::from_raw(guild.channels[0].id);

    let too_long = "x".repeat(4001);
    for bad in ["", "   \n\t ", too_long.as_str()] {
        let err = ctx
            .svc
            .send_message(a, ch, bad.to_owned(), None, 1)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ChatError::InvalidArgument(_)),
            "{} chars -> {err:?}",
            bad.chars().count()
        );
    }

    // Exactly 4000 CHARS (multi-byte) passes validation AND the DB CHECK.
    let content = "é".repeat(4000);
    let msg = ctx.svc.send_message(a, ch, content, None, 5).await.unwrap();
    assert_eq!(msg.content.chars().count(), 4000);

    // Validation fires before channel resolution.
    let err = ctx
        .svc
        .send_message(a, ChannelId::from_raw(1), "y".repeat(4001), None, 5)
        .await
        .unwrap_err();
    assert!(matches!(err, ChatError::InvalidArgument(_)), "{err:?}");

    ctx.finish().await;
}

#[tokio::test]
async fn sync_user_state_builds_the_full_ready_snapshot() {
    let ctx = Ctx::new(2).await;
    let (a, b) = (ctx.users[0], ctx.users[1]);
    let guild = ctx.svc.create_guild(a, "sync-test".into()).await.unwrap();
    ctx.svc.join_guild(b, &guild.invite_code).await.unwrap();
    let extra = ctx
        .svc
        .create_channel(a, GuildId::from_raw(guild.id), "second".into())
        .await
        .unwrap();
    let dm = ctx.svc.open_dm(a, b).await.unwrap();

    let state = ctx.svc.sync_user_state(a).await.unwrap();

    let g = state
        .guilds
        .iter()
        .find(|g| g.id == guild.id)
        .expect("guild present in sync");
    assert_eq!(g.invite_code, guild.invite_code, "members see the code");
    assert_eq!(g.channels.len(), 2);
    assert_eq!(g.channels[0].name, "general");
    assert_eq!(g.channels[0].position, 0);
    assert_eq!(g.channels[1].id, extra.id);
    assert_eq!(g.channels[1].position, 1);
    assert_eq!(g.members.len(), 2);

    let dm_state = state
        .dm_channels
        .iter()
        .find(|c| c.id == dm.id)
        .expect("dm channel present in sync");
    assert_eq!(dm_state.kind, v1::ChannelKind::Dm as i32);
    assert_eq!(dm_state.recipient_ids, sorted(vec![a.raw(), b.raw()]));

    // Deduplicated dictionary: covers self + guild members + DM recipients,
    // no duplicates.
    let ids: Vec<u64> = state.users.iter().map(|u| u.id).collect();
    assert!(ids.contains(&a.raw()), "self present");
    assert!(ids.contains(&b.raw()), "other member present");
    let set: HashSet<u64> = ids.iter().copied().collect();
    assert_eq!(set.len(), ids.len(), "no duplicate users");

    // The other member sees the guild too.
    let state_b = ctx.svc.sync_user_state(b).await.unwrap();
    assert!(state_b.guilds.iter().any(|g2| g2.id == guild.id));
    assert!(state_b.dm_channels.iter().any(|c| c.id == dm.id));

    ctx.finish().await;
}
