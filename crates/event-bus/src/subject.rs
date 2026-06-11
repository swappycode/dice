//! Typed bus subject taxonomy — BINDING, from docs/protocol.md §9.
//!
//! Subjects are constructed only through this enum so they cannot be
//! fat-fingered at publish/subscribe sites. String forms:
//!
//! - `dice.evt.guild.{guild_id}.msg|typing|presence`
//! - `dice.evt.dm.{channel_id}.msg|typing|presence`
//! - `dice.evt.user.{user_id}`

use std::fmt;
use std::str::FromStr;

use dice_common::id::{ChannelId, GuildId, UserId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Subject {
    GuildMsg(GuildId),
    GuildTyping(GuildId),
    GuildPresence(GuildId),
    DmMsg(ChannelId),
    DmTyping(ChannelId),
    DmPresence(ChannelId),
    /// Self-targeted events: GuildCreate, DmChannelCreate, SessionRevoked.
    User(UserId),
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GuildMsg(id) => write!(f, "dice.evt.guild.{id}.msg"),
            Self::GuildTyping(id) => write!(f, "dice.evt.guild.{id}.typing"),
            Self::GuildPresence(id) => write!(f, "dice.evt.guild.{id}.presence"),
            Self::DmMsg(id) => write!(f, "dice.evt.dm.{id}.msg"),
            Self::DmTyping(id) => write!(f, "dice.evt.dm.{id}.typing"),
            Self::DmPresence(id) => write!(f, "dice.evt.dm.{id}.presence"),
            Self::User(id) => write!(f, "dice.evt.user.{id}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid bus subject: {0:?}")]
pub struct SubjectParseError(pub String);

/// Strict decimal id token: ASCII digits only (`u64::from_str` would also
/// accept a leading `+`, which is not a valid subject token).
fn parse_id(token: &str) -> Option<u64> {
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    token.parse().ok()
}

impl FromStr for Subject {
    type Err = SubjectParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || SubjectParseError(s.to_owned());
        let rest = s.strip_prefix("dice.evt.").ok_or_else(err)?;
        let mut parts = rest.split('.');
        let subject = match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some("guild"), Some(id), Some(kind), None) => {
                let id = GuildId::from_raw(parse_id(id).ok_or_else(err)?);
                match kind {
                    "msg" => Self::GuildMsg(id),
                    "typing" => Self::GuildTyping(id),
                    "presence" => Self::GuildPresence(id),
                    _ => return Err(err()),
                }
            }
            (Some("dm"), Some(id), Some(kind), None) => {
                let id = ChannelId::from_raw(parse_id(id).ok_or_else(err)?);
                match kind {
                    "msg" => Self::DmMsg(id),
                    "typing" => Self::DmTyping(id),
                    "presence" => Self::DmPresence(id),
                    _ => return Err(err()),
                }
            }
            (Some("user"), Some(id), None, None) => {
                Self::User(UserId::from_raw(parse_id(id).ok_or_else(err)?))
            }
            _ => return Err(err()),
        };
        Ok(subject)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn display_parse_round_trip_every_variant() {
        let cases = [
            (
                Subject::GuildMsg(GuildId::from_raw(42)),
                "dice.evt.guild.42.msg",
            ),
            (
                Subject::GuildTyping(GuildId::from_raw(42)),
                "dice.evt.guild.42.typing",
            ),
            (
                Subject::GuildPresence(GuildId::from_raw(42)),
                "dice.evt.guild.42.presence",
            ),
            (Subject::DmMsg(ChannelId::from_raw(7)), "dice.evt.dm.7.msg"),
            (
                Subject::DmTyping(ChannelId::from_raw(7)),
                "dice.evt.dm.7.typing",
            ),
            (
                Subject::DmPresence(ChannelId::from_raw(7)),
                "dice.evt.dm.7.presence",
            ),
            (Subject::User(UserId::from_raw(99)), "dice.evt.user.99"),
        ];
        for (subject, wire) in cases {
            assert_eq!(subject.to_string(), wire);
            assert_eq!(wire.parse::<Subject>().unwrap(), subject);
        }
    }

    #[test]
    fn round_trips_max_snowflake() {
        // Largest valid snowflake (bit 63 is always 0).
        let id = u64::MAX >> 1;
        let subject = Subject::User(UserId::from_raw(id));
        assert_eq!(subject.to_string().parse::<Subject>().unwrap(), subject);
    }

    #[test]
    fn rejects_malformed_subjects() {
        let bad = [
            "",
            "dice.evt",
            "dice.evt.",
            "dice.evt.guild.1",                   // missing kind
            "dice.evt.guild.1.nope",              // unknown kind
            "dice.evt.guild.x.msg",               // non-numeric id
            "dice.evt.guild..msg",                // empty id
            "dice.evt.guild.+1.msg",              // u64::from_str would accept this
            "dice.evt.dm.1",                      // missing kind
            "dice.evt.dm.1.msg.extra",            // trailing token
            "dice.evt.user.1.msg",                // user takes no kind
            "dice.evt.user.",                     // empty id
            "dice.evt.user.18446744073709551616", // > u64::MAX
            "evt.guild.1.msg",                    // wrong prefix
            "dice.evt.banana.1.msg",              // unknown scope
        ];
        for s in bad {
            assert!(s.parse::<Subject>().is_err(), "should reject {s:?}");
        }
    }
}
