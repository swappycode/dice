//! Foundation utilities shared by every Dice crate.

pub mod config;
pub mod id;
pub mod shutdown;
pub mod time;

pub use id::{ChannelId, EventId, GuildId, MessageId, SessionId, Snowflake, SnowflakeGenerator, UserId};
