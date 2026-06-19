//! Live-infra integration test for the orphaned-media GC sweep (M4 follow-up):
//! real Postgres + a per-run temp-dir `LocalFsStore`. Mirrors `live.rs`'s setup
//! but keeps the store handle so the sweep can be driven directly.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use dice_common::{SnowflakeGenerator, UserId};
use media_service::{LocalFsStore, Media, MediaError, MediaService, MediaStore, gc};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

fn ids() -> Arc<SnowflakeGenerator> {
    static IDS: OnceLock<Arc<SnowflakeGenerator>> = OnceLock::new();
    IDS.get_or_init(|| {
        let node = (std::process::id() % 1024) as u16;
        Arc::new(SnowflakeGenerator::new(node).unwrap())
    })
    .clone()
}

struct Ctx {
    pool: PgPool,
    svc: MediaService,
    store: Arc<dyn MediaStore>,
    user: UserId,
    dir: PathBuf,
}

impl Ctx {
    async fn new() -> Self {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://dice:dice_dev@localhost:5433/dice".to_owned());
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .expect("live Postgres required (just infra-up)");
        let user = new_user(&pool).await;
        let dir = std::env::temp_dir().join(format!("dice-media-gc-{}", user.raw()));
        let store: Arc<dyn MediaStore> = Arc::new(LocalFsStore::new(dir.clone()));
        let svc = MediaService::new(pool.clone(), store.clone(), ids());
        Self {
            pool,
            svc,
            store,
            user,
            dir,
        }
    }

    async fn finish(self) {
        // media rows cascade from the user (uploader_id FK ON DELETE CASCADE).
        let _ = sqlx::query!("DELETE FROM users WHERE id = $1", self.user.as_i64())
            .execute(&self.pool)
            .await;
        let _ = tokio::fs::remove_dir_all(&self.dir).await;
    }

    async fn media_exists(&self, id: i64) -> bool {
        sqlx::query!("SELECT id FROM media WHERE id = $1", id)
            .fetch_optional(&self.pool)
            .await
            .unwrap()
            .is_some()
    }

    /// Age a media row past the grace window so the sweep is eligible to reap it,
    /// WITHOUT touching the fresh media of concurrently-running tests.
    async fn backdate(&self, id: i64) {
        sqlx::query!(
            "UPDATE media SET created_at = now() - interval '2 hours' WHERE id = $1",
            id
        )
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

async fn new_user(pool: &PgPool) -> UserId {
    let id = UserId::from(ids().generate());
    let name = format!("gc{}", id.raw());
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

/// Orphaned media is grace-protected, then reaped (blob + row) once old enough;
/// re-sweeping is a no-op for it; avatar-referenced media is never reaped.
/// Assertions are per-row (the sweep count is global — other tests may have
/// their own orphans) and the grace stays at 1 h so concurrent tests' fresh
/// uploads are never collateral; this test ages ITS OWN rows to make them due.
#[tokio::test]
async fn gc_reaps_only_unreferenced_media_past_the_grace_window() {
    let ctx = Ctx::new().await;
    let grace = 3600.0;

    // An upload that is never attached to a message or set as an avatar.
    let orphan = ctx
        .svc
        .upload(ctx.user, "orphan.txt", "text/plain", Bytes::from("orphan"))
        .await
        .unwrap();
    let orphan_id = orphan.id.as_i64();

    // Fresh => the grace window protects it.
    gc::sweep(&ctx.pool, &ctx.store, grace).await.unwrap();
    assert!(
        ctx.media_exists(orphan_id).await,
        "fresh orphan survives the grace"
    );

    // Age it past the grace => the sweep reaps the blob AND the row.
    ctx.backdate(orphan_id).await;
    let reaped = gc::sweep(&ctx.pool, &ctx.store, grace).await.unwrap();
    assert!(reaped >= 1, "an aged orphan is reaped");
    assert!(!ctx.media_exists(orphan_id).await, "row deleted");
    assert!(
        matches!(ctx.svc.read(orphan.id).await, Err(MediaError::NotFound)),
        "blob + row both gone"
    );

    // Re-sweeping doesn't resurrect it (idempotent for our row).
    gc::sweep(&ctx.pool, &ctx.store, grace).await.unwrap();
    assert!(!ctx.media_exists(orphan_id).await, "stays reaped");

    // A referenced blob (here an avatar; message_attachments is the symmetric
    // clause) is NEVER reaped, even when aged past the grace.
    let avatar = ctx
        .svc
        .upload(ctx.user, "me.txt", "text/plain", Bytes::from("avatar"))
        .await
        .unwrap();
    sqlx::query!(
        "UPDATE users SET avatar_media_id = $1 WHERE id = $2",
        avatar.id.as_i64(),
        ctx.user.as_i64()
    )
    .execute(&ctx.pool)
    .await
    .unwrap();
    ctx.backdate(avatar.id.as_i64()).await;
    gc::sweep(&ctx.pool, &ctx.store, grace).await.unwrap();
    assert!(
        ctx.media_exists(avatar.id.as_i64()).await,
        "avatar-referenced media is not reaped"
    );

    ctx.finish().await;
}
