//! Snowflake ids (ADR-0004): `[1 bit 0][41 bits ms since DICE_EPOCH][10 bits node][12 bits seq]`.
//! Bit 63 is always 0 so ids fit Postgres BIGINT / JS BigInt / SQLite INTEGER.

use std::fmt;
use std::num::ParseIntError;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::time::DICE_EPOCH_MS;

const TIMESTAMP_BITS: u64 = 41;
const NODE_BITS: u64 = 10;
const SEQ_BITS: u64 = 12;
const MAX_NODE: u16 = (1 << NODE_BITS) as u16 - 1; // 1023
const SEQ_MASK: u64 = (1 << SEQ_BITS) - 1;
const TS_MASK: u64 = (1 << TIMESTAMP_BITS) - 1;

#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct Snowflake(pub u64);

impl Snowflake {
    pub fn timestamp_dice_ms(self) -> u64 {
        (self.0 >> (NODE_BITS + SEQ_BITS)) & TS_MASK
    }

    /// Creation time, ms since the Unix epoch.
    pub fn timestamp_ms(self) -> u64 {
        self.timestamp_dice_ms() + DICE_EPOCH_MS
    }

    pub fn node(self) -> u16 {
        ((self.0 >> SEQ_BITS) & ((1 << NODE_BITS) - 1)) as u16
    }

    /// Database form. Bit 63 is always 0, so this never wraps negative.
    pub fn as_i64(self) -> i64 {
        self.0 as i64
    }

    pub fn from_i64(v: i64) -> Self {
        Self(v as u64)
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for Snowflake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for Snowflake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Snowflake({})", self.0)
    }
}

impl FromStr for Snowflake {
    type Err = ParseIntError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

macro_rules! typed_id {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Snowflake);

        impl $name {
            pub fn raw(self) -> u64 { self.0.0 }
            pub fn as_i64(self) -> i64 { self.0.as_i64() }
            pub fn from_i64(v: i64) -> Self { Self(Snowflake::from_i64(v)) }
            pub fn from_raw(v: u64) -> Self { Self(Snowflake(v)) }
            pub const fn is_zero(self) -> bool { self.0.is_zero() }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0.0)
            }
        }
        impl FromStr for $name {
            type Err = ParseIntError;
            fn from_str(s: &str) -> Result<Self, Self::Err> { Ok(Self(s.parse()?)) }
        }
        impl From<Snowflake> for $name {
            fn from(s: Snowflake) -> Self { Self(s) }
        }
    )+};
}

typed_id!(UserId, GuildId, ChannelId, MessageId, SessionId, EventId);

/// Lock-free generator: one `AtomicU64` packed as `(dice_ts << 12) | seq`.
///
/// Clock regression never produces a smaller timestamp (we keep the stored one
/// and bump seq); 12-bit seq overflow spins to the next millisecond.
pub struct SnowflakeGenerator {
    node: u64,
    state: AtomicU64, // (timestamp << SEQ_BITS) | seq
}

#[derive(Debug, thiserror::Error)]
#[error("node id {0} exceeds the 10-bit maximum ({MAX_NODE})")]
pub struct NodeIdTooLarge(pub u16);

impl SnowflakeGenerator {
    pub fn new(node_id: u16) -> Result<Self, NodeIdTooLarge> {
        if node_id > MAX_NODE {
            return Err(NodeIdTooLarge(node_id));
        }
        Ok(Self {
            node: node_id as u64,
            state: AtomicU64::new(0),
        })
    }

    pub fn generate(&self) -> Snowflake {
        loop {
            let now = crate::time::now_dice_ms() & TS_MASK;
            let cur = self.state.load(Ordering::Acquire);
            let cur_ts = cur >> SEQ_BITS;
            let (ts, seq) = if now > cur_ts {
                (now, 0)
            } else {
                // Same millisecond, or the clock went backwards: stay on the
                // stored timestamp and take the next sequence slot.
                let next_seq = (cur & SEQ_MASK) + 1;
                if next_seq > SEQ_MASK {
                    // 4096 ids in one ms on one node: wait for the clock.
                    std::hint::spin_loop();
                    continue;
                }
                (cur_ts, next_seq)
            };
            let next = (ts << SEQ_BITS) | seq;
            if self
                .state
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Snowflake((ts << (NODE_BITS + SEQ_BITS)) | (self.node << SEQ_BITS) | seq);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn bit_63_always_zero_and_fields_round_trip() {
        let g = SnowflakeGenerator::new(1023).unwrap();
        for _ in 0..10_000 {
            let id = g.generate();
            assert_eq!(id.0 >> 63, 0);
            assert_eq!(id.node(), 1023);
            assert!(id.as_i64() > 0);
        }
    }

    #[test]
    fn strictly_monotonic_single_thread() {
        let g = SnowflakeGenerator::new(0).unwrap();
        let mut last = Snowflake(0);
        for _ in 0..50_000 {
            let id = g.generate();
            assert!(id > last, "ids must be strictly increasing");
            last = id;
        }
    }

    #[test]
    fn unique_across_threads() {
        use std::collections::HashSet;
        use std::sync::Arc;
        let g = Arc::new(SnowflakeGenerator::new(7).unwrap());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let g = Arc::clone(&g);
                std::thread::spawn(move || (0..10_000).map(|_| g.generate().0).collect::<Vec<_>>())
            })
            .collect();
        let mut seen = HashSet::new();
        for h in handles {
            for id in h.join().unwrap() {
                assert!(seen.insert(id), "duplicate snowflake {id}");
            }
        }
    }

    #[test]
    fn node_id_range_enforced() {
        assert!(SnowflakeGenerator::new(1024).is_err());
    }

    #[test]
    fn decimal_string_round_trip() {
        let id = Snowflake(123_456_789_012_345);
        let s = id.to_string();
        assert_eq!(s.parse::<Snowflake>().unwrap(), id);
        let uid: UserId = s.parse().unwrap();
        assert_eq!(uid.raw(), id.0);
    }
}
