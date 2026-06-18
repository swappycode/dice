//! Client-side counters + latency histograms. All hot-path updates are
//! `Relaxed` atomics (these are observability, not synchronization). The reporter
//! reads snapshots; correlate these with the gateway's own
//! `dice_gateway_connections{transport}` / `dice_gateway_closes_total{code}` and
//! its RSS/CPU (the server side of the same run).

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Coarse fixed-bucket histogram (upper bounds in ms) + count/sum for the mean.
/// Approximate percentiles are plenty for a load-test report and cost one atomic
/// add per sample with no allocation or lock.
struct Histogram {
    buckets: [AtomicU64; Self::N],
    count: AtomicU64,
    sum: AtomicU64,
}

impl Histogram {
    const BOUNDS_MS: [u64; 13] = [
        1, 2, 5, 10, 20, 50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000,
    ];
    const N: usize = Self::BOUNDS_MS.len() + 1; // + overflow bucket

    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0),
        }
    }

    fn record(&self, ms: u64) {
        let idx = Self::BOUNDS_MS
            .iter()
            .position(|&b| ms <= b)
            .unwrap_or(Self::BOUNDS_MS.len());
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(ms, Ordering::Relaxed);
    }

    /// Approximate percentile: the upper bound of the bucket the p-th sample
    /// falls in (overflow bucket reports as `>10000`).
    fn percentile(&self, p: u8) -> u64 {
        let total = self.count.load(Ordering::Relaxed);
        if total == 0 {
            return 0;
        }
        let target = (total.saturating_mul(p as u64)) / 100;
        let mut cumulative = 0u64;
        for (i, b) in self.buckets.iter().enumerate() {
            cumulative += b.load(Ordering::Relaxed);
            if cumulative >= target.max(1) {
                return Self::BOUNDS_MS.get(i).copied().unwrap_or(u64::MAX);
            }
        }
        u64::MAX
    }

    fn mean(&self) -> u64 {
        self.sum
            .load(Ordering::Relaxed)
            .checked_div(self.count.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

/// The whole-run counter set.
pub struct Stats {
    pub attempted: AtomicU64,
    pub established_total: AtomicU64,
    /// Currently-live established connections (the client-side mirror of the
    /// gateway connection gauge).
    pub live: AtomicI64,
    pub connect_failed: AtomicU64,
    pub handshake_failed: AtomicU64,
    pub disconnected: AtomicU64,
    pub hb_sent: AtomicU64,
    pub hb_acked: AtomicU64,
    // Disconnect close-code breakdown (the resumable/auth codes worth watching).
    closed_heartbeat_timeout: AtomicU64, // 4012
    closed_slow_consumer: AtomicU64,     // 4010
    closed_going_away: AtomicU64,        // 4011
    closed_unauthenticated: AtomicU64,   // 4001
    closed_other: AtomicU64,
    connect_ms: Histogram,
    rtt_ms: Histogram,
}

impl Stats {
    pub fn new() -> Self {
        Self {
            attempted: AtomicU64::new(0),
            established_total: AtomicU64::new(0),
            live: AtomicI64::new(0),
            connect_failed: AtomicU64::new(0),
            handshake_failed: AtomicU64::new(0),
            disconnected: AtomicU64::new(0),
            hb_sent: AtomicU64::new(0),
            hb_acked: AtomicU64::new(0),
            closed_heartbeat_timeout: AtomicU64::new(0),
            closed_slow_consumer: AtomicU64::new(0),
            closed_going_away: AtomicU64::new(0),
            closed_unauthenticated: AtomicU64::new(0),
            closed_other: AtomicU64::new(0),
            connect_ms: Histogram::new(),
            rtt_ms: Histogram::new(),
        }
    }

    pub fn attempt(&self) {
        self.attempted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn connect_failed(&self) {
        self.connect_failed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn handshake_failed(&self) {
        self.handshake_failed.fetch_add(1, Ordering::Relaxed);
    }

    /// A connection reached Ready; record its end-to-end establish latency.
    pub fn established(&self, connect_ms: u64) {
        self.established_total.fetch_add(1, Ordering::Relaxed);
        self.live.fetch_add(1, Ordering::Relaxed);
        self.connect_ms.record(connect_ms);
    }

    /// A live connection ended (any reason); drops the live gauge.
    pub fn ended(&self) {
        self.live.fetch_sub(1, Ordering::Relaxed);
    }

    /// A live connection dropped unexpectedly (not our shutdown), with the close
    /// code if the peer sent one.
    pub fn disconnected(&self, code: Option<u32>) {
        self.disconnected.fetch_add(1, Ordering::Relaxed);
        let bucket = match code {
            Some(4012) => &self.closed_heartbeat_timeout,
            Some(4010) => &self.closed_slow_consumer,
            Some(4011) => &self.closed_going_away,
            Some(4001) => &self.closed_unauthenticated,
            _ => &self.closed_other,
        };
        bucket.fetch_add(1, Ordering::Relaxed);
    }

    pub fn hb_sent(&self) {
        self.hb_sent.fetch_add(1, Ordering::Relaxed);
    }

    pub fn hb_rtt(&self, rtt_ms: u64) {
        self.hb_acked.fetch_add(1, Ordering::Relaxed);
        self.rtt_ms.record(rtt_ms);
    }

    /// One-line snapshot for the periodic/final report.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            attempted: self.attempted.load(Ordering::Relaxed),
            established_total: self.established_total.load(Ordering::Relaxed),
            live: self.live.load(Ordering::Relaxed).max(0) as u64,
            connect_failed: self.connect_failed.load(Ordering::Relaxed),
            handshake_failed: self.handshake_failed.load(Ordering::Relaxed),
            disconnected: self.disconnected.load(Ordering::Relaxed),
            hb_sent: self.hb_sent.load(Ordering::Relaxed),
            hb_acked: self.hb_acked.load(Ordering::Relaxed),
            connect_p50: self.connect_ms.percentile(50),
            connect_p99: self.connect_ms.percentile(99),
            connect_mean: self.connect_ms.mean(),
            rtt_p50: self.rtt_ms.percentile(50),
            rtt_p99: self.rtt_ms.percentile(99),
        }
    }

    /// The disconnect close-code breakdown (only printed when non-zero).
    pub fn close_breakdown(&self) -> String {
        let parts = [
            ("hb_timeout(4012)", &self.closed_heartbeat_timeout),
            ("slow_consumer(4010)", &self.closed_slow_consumer),
            ("going_away(4011)", &self.closed_going_away),
            ("unauth(4001)", &self.closed_unauthenticated),
            ("other", &self.closed_other),
        ];
        parts
            .iter()
            .filter_map(|(name, c)| {
                let v = c.load(Ordering::Relaxed);
                (v > 0).then(|| format!("{name}={v}"))
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Point-in-time view used by the reporter.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    pub attempted: u64,
    pub established_total: u64,
    pub live: u64,
    pub connect_failed: u64,
    pub handshake_failed: u64,
    pub disconnected: u64,
    pub hb_sent: u64,
    pub hb_acked: u64,
    pub connect_p50: u64,
    pub connect_p99: u64,
    pub connect_mean: u64,
    pub rtt_p50: u64,
    pub rtt_p99: u64,
}

fn fmt_ms(ms: u64) -> String {
    if ms == u64::MAX {
        ">10000ms".to_owned()
    } else {
        format!("{ms}ms")
    }
}

impl Snapshot {
    /// A compact human line for the periodic report.
    pub fn line(&self, elapsed_s: f64) -> String {
        format!(
            "t={elapsed_s:6.1}s live={live} established={est} attempted={att} \
             fail(connect={cf} handshake={hf}) disconnected={dc} \
             connect(p50={c50} p99={c99} mean={cmean}) rtt(p50={r50} p99={r99}) hb(sent={hs} ack={ha})",
            live = self.live,
            est = self.established_total,
            att = self.attempted,
            cf = self.connect_failed,
            hf = self.handshake_failed,
            dc = self.disconnected,
            c50 = fmt_ms(self.connect_p50),
            c99 = fmt_ms(self.connect_p99),
            cmean = fmt_ms(self.connect_mean),
            r50 = fmt_ms(self.rtt_p50),
            r99 = fmt_ms(self.rtt_p99),
            hs = self.hb_sent,
            ha = self.hb_acked,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_percentiles_track_the_distribution() {
        let h = Histogram::new();
        // 95 fast + 5 slow: a clean p50-fast / p99-tail split (small-N percentiles
        // truncate, so use a realistic sample count).
        for _ in 0..95 {
            h.record(1);
        }
        for _ in 0..5 {
            h.record(9_999);
        }
        assert!(
            h.percentile(50) <= 50,
            "p50 should be small, got {}",
            h.percentile(50)
        );
        assert!(
            h.percentile(99) >= 5_000,
            "p99 should be in the tail, got {}",
            h.percentile(99)
        );
        assert!(h.mean() > 0);
    }

    #[test]
    fn empty_histogram_is_zero() {
        let h = Histogram::new();
        assert_eq!(h.percentile(50), 0);
        assert_eq!(h.mean(), 0);
    }

    #[test]
    fn close_codes_bucket_correctly() {
        let s = Stats::new();
        s.disconnected(Some(4012));
        s.disconnected(Some(4010));
        s.disconnected(None);
        let breakdown = s.close_breakdown();
        assert!(breakdown.contains("hb_timeout(4012)=1"));
        assert!(breakdown.contains("slow_consumer(4010)=1"));
        assert!(breakdown.contains("other=1"));
        assert_eq!(s.disconnected.load(Ordering::Relaxed), 3);
    }
}
