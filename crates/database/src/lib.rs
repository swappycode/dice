//! `dice-database`: Postgres pool bootstrap + embedded migrations.
//!
//! One database, one linear migration history (`crates/database/migrations/`),
//! embedded via [`sqlx::migrate!`] so the monolith self-migrates on boot — the
//! self-hosting story is one binary plus one Postgres. Migration files are
//! namespaced by owning service; services keep their *queries* (the
//! `sqlx::query!` macros) in their own crates. This crate deliberately exposes
//! no query helpers.
//!
//! ADR-0003: Postgres is always required; there is no in-memory fallback.

use std::time::Duration;

use dice_common::config::{ConfigError, env_or, require};
use sqlx::postgres::PgPoolOptions;

pub use sqlx::PgPool;

/// Default for `DICE_DB_MAX_CONNS` (dev-sized; prod overrides via env).
pub const DEFAULT_MAX_CONNECTIONS: u32 = 16;

/// Pool acquire timeout. Not env-configurable in M1: a connection that cannot
/// be acquired in 5 s means the pool is sized wrong or Postgres is down, and
/// failing fast beats queueing request handlers.
pub const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Connection-pool configuration, sourced from the environment.
#[derive(Debug, Clone)]
pub struct DbConfig {
    /// Postgres URL (`DATABASE_URL`), e.g. `postgres://dice:dice@localhost:5432/dice`.
    pub url: String,
    /// Pool cap (`DICE_DB_MAX_CONNS`, default 16).
    pub max_connections: u32,
    /// How long `acquire()` waits for a free connection (fixed 5 s).
    pub acquire_timeout: Duration,
}

impl DbConfig {
    /// Read configuration from the environment.
    ///
    /// `DATABASE_URL` is required; `DICE_DB_MAX_CONNS` defaults to
    /// [`DEFAULT_MAX_CONNECTIONS`]; the acquire timeout is fixed at
    /// [`DEFAULT_ACQUIRE_TIMEOUT`].
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Self {
            url: require("DATABASE_URL")?,
            max_connections: env_or("DICE_DB_MAX_CONNS", DEFAULT_MAX_CONNECTIONS)?,
            acquire_timeout: DEFAULT_ACQUIRE_TIMEOUT,
        })
    }
}

/// Open a [`PgPool`] for `cfg`. Establishes at least one connection before
/// returning, so a bad URL or unreachable server fails here, not on first use.
pub async fn connect(cfg: &DbConfig) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(cfg.max_connections)
        .acquire_timeout(cfg.acquire_timeout)
        .connect(&cfg.url)
        .await
}

/// The embedded migrator: the full M1 schema, compiled into the binary.
/// Exposed for tests and for the monolith's self-migrate-on-boot path.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Run all pending migrations from [`MIGRATOR`] against `pool`.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    MIGRATOR.run(pool).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn migrator_embeds_migrations_with_strictly_increasing_versions() {
        let versions: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
        assert_eq!(
            versions.len(),
            9,
            "expected 9 embedded migrations (M1 4 + M2 replies/reactions/attachments/avatars/read-markers), got {versions:?}"
        );
        assert!(
            versions.windows(2).all(|w| w[0] < w[1]),
            "migration versions must be strictly increasing, got {versions:?}"
        );
    }

    #[test]
    fn migration_descriptions_match_the_schema_files() {
        let descriptions: Vec<&str> = MIGRATOR.iter().map(|m| m.description.as_ref()).collect();
        assert_eq!(
            descriptions,
            [
                "users",
                "auth sessions",
                "guilds channels",
                "messages",
                "replies",
                "reactions",
                "attachments",
                "avatars",
                "read markers"
            ]
        );
    }

    #[test]
    fn default_pool_sizing_constants() {
        assert_eq!(DEFAULT_MAX_CONNECTIONS, 16);
        assert_eq!(DEFAULT_ACQUIRE_TIMEOUT, Duration::from_secs(5));
    }

    /// Needs live Postgres: run `just infra-up` and set `DATABASE_URL`
    /// (e.g. postgres://dice:dice@localhost:5432/dice), then
    /// `cargo test -p dice-database -- --ignored`.
    #[tokio::test]
    #[ignore = "requires live Postgres (just infra-up + DATABASE_URL)"]
    async fn migrate_creates_all_tables() {
        let cfg = DbConfig::from_env().expect("DATABASE_URL must be set for this test");
        let pool = connect(&cfg).await.expect("connect to Postgres");
        migrate(&pool).await.expect("run embedded migrations");

        let expected: Vec<String> = [
            "users",
            "auth_sessions",
            "refresh_tokens",
            "guilds",
            "guild_members",
            "channels",
            "channel_recipients",
            "messages",
            "message_reactions",
            "media",
            "message_attachments",
            "read_markers",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();

        let present: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_name = ANY($1)",
        )
        .bind(&expected)
        .fetch_one(&pool)
        .await
        .expect("query information_schema");

        assert_eq!(present, 12, "all 12 tables must exist after migrate()");
    }
}
