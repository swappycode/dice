//! Live-infra integration tests: real Postgres (DATABASE_URL, default
//! `postgres://dice:dice_dev@localhost:5433/dice`) + a per-run temp-dir
//! `LocalFsStore`. Each test mints a fresh user (random-ish node id from the
//! PID) and deletes its rows + temp dir at the end (best-effort).

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use dice_common::{MediaId, SnowflakeGenerator, UserId};
use media_service::{LocalFsStore, Media, MediaError, MediaService};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// A minimal but valid 1×1 PNG (signature + IHDR through CRC). `imagesize`
/// reads width/height straight from the IHDR, so this is enough to sniff 1×1.
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
    0x00, 0x00, 0x00, 0x0D, // IHDR length = 13
    0x49, 0x48, 0x44, 0x52, // "IHDR"
    0x00, 0x00, 0x00, 0x01, // width = 1
    0x00, 0x00, 0x00, 0x01, // height = 1
    0x08, 0x06, 0x00, 0x00, 0x00, // bit depth, color type, compression, filter, interlace
    0x1F, 0x15, 0xC4, 0x89, // IHDR CRC
];

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
        let dir = std::env::temp_dir().join(format!("dice-media-test-{}", user.raw()));
        let store = Arc::new(LocalFsStore::new(dir.clone()));
        // A small cap so the oversize path is cheap to exercise.
        let svc = MediaService::new(pool.clone(), store, ids()).with_max_bytes(1024);
        Self {
            pool,
            svc,
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
}

async fn new_user(pool: &PgPool) -> UserId {
    let id = UserId::from(ids().generate());
    let name = format!("md{}", id.raw());
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

#[tokio::test]
async fn upload_image_records_dimensions_and_round_trips() {
    let ctx = Ctx::new().await;
    let obj = ctx
        .svc
        .upload(
            ctx.user,
            "art/cat.png",
            "image/png",
            Bytes::from_static(PNG_1X1),
        )
        .await
        .unwrap();
    assert_eq!(obj.filename, "cat.png", "basename only");
    assert_eq!(obj.content_type, "image/png");
    assert_eq!(obj.size_bytes, PNG_1X1.len() as u64);
    assert_eq!((obj.width, obj.height), (1, 1), "sniffed from IHDR");
    assert_eq!(obj.uploader, ctx.user);

    let (meta, bytes) = ctx.svc.read(obj.id).await.unwrap();
    assert_eq!(meta, obj, "metadata round-trips");
    assert_eq!(bytes.as_ref(), PNG_1X1, "bytes round-trip from the store");

    // The wire form mirrors the object.
    let att = obj.to_attachment();
    assert_eq!(att.id, obj.id.raw());
    assert_eq!((att.width, att.height), (1, 1));

    ctx.finish().await;
}

#[tokio::test]
async fn upload_non_image_has_zero_dimensions() {
    let ctx = Ctx::new().await;
    let obj = ctx
        .svc
        .upload(
            ctx.user,
            "notes.txt",
            "text/plain",
            Bytes::from_static(b"hello dice"),
        )
        .await
        .unwrap();
    assert_eq!(
        (obj.width, obj.height),
        (0, 0),
        "non-image => no dimensions"
    );
    let (_, bytes) = ctx.svc.read(obj.id).await.unwrap();
    assert_eq!(bytes.as_ref(), b"hello dice");
    ctx.finish().await;
}

#[tokio::test]
async fn upload_rejects_empty_oversize_and_corrupt_image() {
    let ctx = Ctx::new().await;

    let err = ctx
        .svc
        .upload(ctx.user, "x", "text/plain", Bytes::new())
        .await
        .unwrap_err();
    assert!(matches!(err, MediaError::InvalidArgument(_)), "{err:?}");

    // Cap is 1024 in the test ctx.
    let err = ctx
        .svc
        .upload(
            ctx.user,
            "big.bin",
            "application/octet-stream",
            Bytes::from(vec![0u8; 2048]),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MediaError::TooLarge { max: 1024 }), "{err:?}");

    // Declares an image but the bytes are not a valid image.
    let err = ctx
        .svc
        .upload(
            ctx.user,
            "fake.png",
            "image/png",
            Bytes::from_static(b"not really a png"),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MediaError::InvalidArgument(_)), "{err:?}");

    ctx.finish().await;
}

#[tokio::test]
async fn read_unknown_is_not_found() {
    let ctx = Ctx::new().await;
    let err = ctx.svc.read(MediaId::from_raw(1)).await.unwrap_err();
    assert!(matches!(err, MediaError::NotFound), "{err:?}");
    ctx.finish().await;
}
