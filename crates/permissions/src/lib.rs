//! Guild permission model for Dice.
//!
//! A `u64` bitfield with the **canonical bit layout** fixed by the integration
//! critique (#17). This layout is the single source of truth: the database
//! stores `to_db()` values verbatim in `BIGINT` columns (no SQL magic-number
//! defaults), and every service imports this crate rather than redefining bits.
//!
//! Semantics:
//! - `ADMINISTRATOR` (bit 63) implies every permission. Note that bit 63 set
//!   makes the `i64` database representation **negative** — that is expected
//!   and lossless (`BIGINT` is a two's-complement i64).
//! - The guild owner always has every permission, regardless of stored grants
//!   (see [`compute`]).
//!
//! No async, no IO — this crate is pure data.

use bitflags::bitflags;

bitflags! {
    /// A set of guild permissions packed into a `u64`.
    ///
    /// Bit layout is canonical and append-only: bits are never renumbered or
    /// reused. Bits 9..=62 are reserved for future permissions.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct Permissions: u64 {
        /// Read a channel and its message history.
        const VIEW_CHANNEL    = 1 << 0;
        /// Send messages in channels the holder can view.
        const SEND_MESSAGES   = 1 << 1;
        /// Delete or moderate other members' messages.
        const MANAGE_MESSAGES = 1 << 2;
        /// Create, edit, and delete channels.
        const MANAGE_CHANNELS = 1 << 3;
        /// Edit guild-level settings.
        const MANAGE_GUILD    = 1 << 4;
        /// Remove members from the guild.
        const KICK_MEMBERS    = 1 << 5;
        /// Ban members from the guild.
        const BAN_MEMBERS     = 1 << 6;
        /// Create invites to the guild.
        const CREATE_INVITE   = 1 << 7;
        /// Create, edit, assign, and delete roles.
        const MANAGE_ROLES    = 1 << 8;
        /// Implies every permission, present and future. Top bit — makes the
        /// `i64` database form negative.
        const ADMINISTRATOR   = 1 << 63;
    }
}

/// Default permission set granted to every member (`@everyone`).
///
/// `VIEW_CHANNEL | SEND_MESSAGES | CREATE_INVITE` = `0b1000_0011` = 131.
/// Migrations must take this value from Rust (`DEFAULT_EVERYONE.to_db()`),
/// never hardcode it in SQL.
pub const DEFAULT_EVERYONE: Permissions = Permissions::VIEW_CHANNEL
    .union(Permissions::SEND_MESSAGES)
    .union(Permissions::CREATE_INVITE);

/// Error returned by [`Permissions::check`] when the holder lacks one or more
/// of the required permissions. Carries exactly the missing bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("missing permissions: {missing:?}")]
pub struct MissingPermissions {
    /// The required permissions the holder does not have.
    pub missing: Permissions,
}

/// Compute a member's effective permissions from ownership and their grants
/// (today: the single per-member grant; signature is forward-proof for roles
/// and channel overwrites).
///
/// The guild owner, or any grant containing [`Permissions::ADMINISTRATOR`],
/// yields [`Permissions::all`]. Otherwise the result is the union of all
/// grants.
pub fn compute(is_owner: bool, grants: impl IntoIterator<Item = Permissions>) -> Permissions {
    if is_owner {
        return Permissions::all();
    }
    let mut effective = Permissions::empty();
    for grant in grants {
        if grant.contains(Permissions::ADMINISTRATOR) {
            return Permissions::all();
        }
        effective |= grant;
    }
    effective
}

impl Permissions {
    /// Check that `self` covers every bit in `required`.
    ///
    /// [`Permissions::ADMINISTRATOR`] implies everything, so an administrator
    /// passes any check. On failure the error carries exactly the bits that
    /// were required but not held.
    pub fn check(self, required: Permissions) -> Result<(), MissingPermissions> {
        if self.contains(Permissions::ADMINISTRATOR) {
            return Ok(());
        }
        let missing = required.difference(self);
        if missing.is_empty() {
            Ok(())
        } else {
            Err(MissingPermissions { missing })
        }
    }

    /// Decode a Postgres `BIGINT` permissions value.
    ///
    /// Lossless: unknown bits (written by a newer deploy) are retained so a
    /// read-modify-write by an older binary never silently drops grants.
    #[must_use]
    pub const fn from_db(v: i64) -> Self {
        // Plain two's-complement reinterpretation: i64 and u64 are the same
        // 64 bits, BIGINT stores them verbatim.
        #[allow(clippy::cast_sign_loss)]
        Self::from_bits_retain(v as u64)
    }

    /// Encode for a Postgres `BIGINT` column.
    ///
    /// If [`Permissions::ADMINISTRATOR`] (bit 63) is set the result is
    /// negative; that is expected and round-trips exactly via [`from_db`].
    ///
    /// [`from_db`]: Permissions::from_db
    #[must_use]
    pub const fn to_db(self) -> i64 {
        #[allow(clippy::cast_possible_wrap)]
        {
            self.bits() as i64
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn bit_layout_is_canonical() {
        // BINDING layout from integration critique #17 — never renumber.
        assert_eq!(Permissions::VIEW_CHANNEL.bits(), 1 << 0);
        assert_eq!(Permissions::SEND_MESSAGES.bits(), 1 << 1);
        assert_eq!(Permissions::MANAGE_MESSAGES.bits(), 1 << 2);
        assert_eq!(Permissions::MANAGE_CHANNELS.bits(), 1 << 3);
        assert_eq!(Permissions::MANAGE_GUILD.bits(), 1 << 4);
        assert_eq!(Permissions::KICK_MEMBERS.bits(), 1 << 5);
        assert_eq!(Permissions::BAN_MEMBERS.bits(), 1 << 6);
        assert_eq!(Permissions::CREATE_INVITE.bits(), 1 << 7);
        assert_eq!(Permissions::MANAGE_ROLES.bits(), 1 << 8);
        assert_eq!(Permissions::ADMINISTRATOR.bits(), 1 << 63);
    }

    #[test]
    fn default_everyone_is_131() {
        assert_eq!(DEFAULT_EVERYONE.to_db(), 131);
        assert_eq!(
            DEFAULT_EVERYONE,
            Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::CREATE_INVITE
        );
    }

    #[test]
    fn administrator_db_value_is_negative_and_round_trips() {
        let admin = Permissions::ADMINISTRATOR;
        let raw = admin.to_db();
        assert!(raw < 0, "bit 63 must make the BIGINT negative, got {raw}");
        assert_eq!(raw, i64::MIN); // exactly 1 << 63 as two's complement
        assert_eq!(Permissions::from_db(raw), admin);

        // Mixed set with the top bit also round-trips.
        let mixed = Permissions::ADMINISTRATOR | DEFAULT_EVERYONE;
        assert!(mixed.to_db() < 0);
        assert_eq!(Permissions::from_db(mixed.to_db()), mixed);
    }

    #[test]
    fn db_round_trip_all_known_sets() {
        for set in [
            Permissions::empty(),
            Permissions::all(),
            DEFAULT_EVERYONE,
            Permissions::MANAGE_ROLES | Permissions::BAN_MEMBERS,
        ] {
            assert_eq!(Permissions::from_db(set.to_db()), set);
        }
    }

    #[test]
    fn from_db_retains_unknown_bits() {
        // A newer deploy may have written bits this binary does not define
        // (e.g. bit 9). Reading and writing back must not drop them.
        let raw = (1_i64 << 9) | DEFAULT_EVERYONE.to_db();
        let perms = Permissions::from_db(raw);
        assert_eq!(perms.to_db(), raw);
        // Known bits still behave.
        assert!(perms.contains(Permissions::VIEW_CHANNEL));
        assert!(!perms.contains(Permissions::ADMINISTRATOR));
    }

    #[test]
    fn compute_owner_gets_all() {
        assert_eq!(compute(true, []), Permissions::all());
        // Owner wins even with empty / restrictive grants present.
        assert_eq!(compute(true, [Permissions::empty()]), Permissions::all());
    }

    #[test]
    fn compute_administrator_grant_gets_all() {
        let grants = [DEFAULT_EVERYONE, Permissions::ADMINISTRATOR];
        assert_eq!(compute(false, grants), Permissions::all());
        // ADMINISTRATOR mixed into a larger grant also triggers it.
        let grants = [Permissions::ADMINISTRATOR | Permissions::VIEW_CHANNEL];
        assert_eq!(compute(false, grants), Permissions::all());
    }

    #[test]
    fn compute_unions_plain_grants() {
        let grants = [DEFAULT_EVERYONE, Permissions::MANAGE_MESSAGES];
        assert_eq!(
            compute(false, grants),
            DEFAULT_EVERYONE | Permissions::MANAGE_MESSAGES
        );
        assert_eq!(compute(false, []), Permissions::empty());
    }

    #[test]
    fn check_passes_when_covered() {
        assert_eq!(DEFAULT_EVERYONE.check(Permissions::SEND_MESSAGES), Ok(()));
        // Empty requirement always passes.
        assert_eq!(Permissions::empty().check(Permissions::empty()), Ok(()));
    }

    #[test]
    fn check_reports_exact_missing_bits() {
        let required =
            Permissions::SEND_MESSAGES | Permissions::MANAGE_GUILD | Permissions::BAN_MEMBERS;
        let err = DEFAULT_EVERYONE.check(required).unwrap_err();
        assert_eq!(
            err.missing,
            Permissions::MANAGE_GUILD | Permissions::BAN_MEMBERS
        );
    }

    #[test]
    fn administrator_passes_every_check() {
        assert_eq!(Permissions::ADMINISTRATOR.check(Permissions::all()), Ok(()));
        assert_eq!(
            Permissions::ADMINISTRATOR.check(Permissions::MANAGE_ROLES | Permissions::KICK_MEMBERS),
            Ok(())
        );
    }

    #[test]
    fn missing_permissions_display_names_the_bits() {
        let err = MissingPermissions {
            missing: Permissions::MANAGE_GUILD | Permissions::BAN_MEMBERS,
        };
        let msg = err.to_string();
        assert!(msg.contains("MANAGE_GUILD"), "got: {msg}");
        assert!(msg.contains("BAN_MEMBERS"), "got: {msg}");
    }
}
