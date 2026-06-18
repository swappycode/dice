//! Offline token minting. The gateway verifies access JWTs cryptographically at
//! Identify with its PUBLIC key only — no DB lookup, no auth-service hop, and a
//! token for a user that doesn't exist in Postgres is accepted (sync_user_state
//! returns an empty Ready; presence.connect is cache-only). So the harness mints
//! its own valid tokens from the gateway's dev signing key and never touches the
//! database — the right identity strategy for 100k synthetic connections
//! (docs/protocol.md §12; ADR-0004 snowflakes).

use std::path::Path;

use anyhow::Context as _;
use dice_auth_core::token::{JwtKeys, sign_access};
use dice_common::id::{SessionId, SnowflakeGenerator, UserId};

/// Holds the loaded signing key + a snowflake source for synthetic identities.
pub struct Identities {
    keys: JwtKeys,
    ids: SnowflakeGenerator,
}

impl Identities {
    /// Load the gateway's dev Ed25519 pair (PKCS#8 private + SPKI public). The
    /// PRIVATE half is required — the gateway verifies with the matching public
    /// half it loaded from the same `dev/keys/` directory.
    pub fn load(private: &Path, public: &Path, node_id: u16) -> anyhow::Result<Self> {
        let private_pem = std::fs::read(private)
            .with_context(|| format!("read JWT private key {}", private.display()))?;
        let public_pem = std::fs::read(public)
            .with_context(|| format!("read JWT public key {}", public.display()))?;
        let keys = JwtKeys::from_pem(&private_pem, &public_pem)
            .context("load Ed25519 JWT keys (need PKCS#8 private + SPKI public PEM)")?;
        anyhow::ensure!(
            keys.can_sign(),
            "loaded a verify-only key — point DICE_LOADGEN_JWT_PRIVATE at the PRIVATE pem"
        );
        let ids = SnowflakeGenerator::new(node_id).context("snowflake node id")?;
        Ok(Self { keys, ids })
    }

    /// Mint a fresh access token for a brand-new synthetic user + auth session.
    ///
    /// The token is only checked once, at Identify, and its 10-minute TTL only
    /// has to be valid at connect time — so even a multi-hour soak just needs a
    /// fresh mint per connection (cheap: one EdDSA signature).
    pub fn mint(&self) -> anyhow::Result<String> {
        let user: UserId = self.ids.generate().into();
        let session: SessionId = self.ids.generate().into();
        sign_access(&self.keys, user, session).context("sign access token")
    }
}
