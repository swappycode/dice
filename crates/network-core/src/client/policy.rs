//! Transport-selection policy (docs/design/desktop-client.md §1.3): try QUIC
//! with a short budget, fall back to WSS within the SAME backoff attempt;
//! after [`QUIC_FAILURES_BEFORE_WSS`] consecutive QUIC connect failures
//! prefer WSS for the rest of the session, re-probing QUIC opportunistically
//! on every [`QUIC_REPROBE_PERIOD`]th later attempt. The host feeds the
//! last-good transport back as [`PreferredTransport`] so a session that
//! ended on WSS does not pay the QUIC timeout again at startup.

use std::time::Duration;

use super::transport::TransportKind;

/// Default QUIC connect budget for [`TransportPolicy::QuicFirst`] (design
/// §1.3: UDP is commonly blocked on corp networks; fail fast to WSS).
pub const DEFAULT_QUIC_TIMEOUT: Duration = Duration::from_secs(3);

/// Consecutive QUIC connect failures before WSS becomes preferred.
pub(crate) const QUIC_FAILURES_BEFORE_WSS: u32 = 2;

/// While WSS is preferred, every Nth connect attempt re-probes QUIC.
pub(crate) const QUIC_REPROBE_PERIOD: u32 = 4;

/// Which transport(s) the gateway driver may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportPolicy {
    /// Attempt QUIC with `quic_timeout`; on failure fall back to WSS within
    /// the same backoff attempt (the default, with [`DEFAULT_QUIC_TIMEOUT`]).
    QuicFirst {
        quic_timeout: Duration,
    },
    WssOnly,
    QuicOnly,
}

impl Default for TransportPolicy {
    fn default() -> Self {
        Self::QuicFirst {
            quic_timeout: DEFAULT_QUIC_TIMEOUT,
        }
    }
}

/// Last-good transport from a previous session (the host persists it in the
/// cache `meta` table and feeds it back on the next connect).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreferredTransport {
    Quic,
    Wss,
}

impl PreferredTransport {
    /// Stable lowercase name for persistence (`"quic"` / `"wss"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quic => "quic",
            Self::Wss => "wss",
        }
    }

    /// Inverse of [`Self::as_str`]; anything unknown is `None`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "quic" => Some(Self::Quic),
            "wss" => Some(Self::Wss),
            _ => None,
        }
    }
}

impl From<TransportKind> for PreferredTransport {
    fn from(kind: TransportKind) -> Self {
        match kind {
            TransportKind::Quic => Self::Quic,
            TransportKind::Wss => Self::Wss,
        }
    }
}

/// What one connect attempt should try (decided per attempt by
/// [`TransportSelector::plan`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectPlan {
    Wss,
    Quic,
    QuicThenWss,
}

/// The policy state machine. Pure (no I/O) so the rules are unit-testable:
/// the driver calls [`Self::plan`] once per connect attempt and reports QUIC
/// connect outcomes back via `note_quic_*`.
#[derive(Debug)]
pub(crate) struct TransportSelector {
    policy: TransportPolicy,
    quic_configured: bool,
    consecutive_quic_failures: u32,
    /// QUIC was demoted (or the host's last-good transport was WSS).
    prefer_wss: bool,
    /// Connect attempts since the demotion, for the re-probe cadence.
    attempts_since_demotion: u32,
}

impl TransportSelector {
    pub(crate) fn new(
        policy: TransportPolicy,
        quic_configured: bool,
        initial: Option<PreferredTransport>,
    ) -> Self {
        Self {
            policy,
            quic_configured,
            consecutive_quic_failures: 0,
            // A WSS preference fed in by the host starts the session in the
            // demoted state: straight to WSS, with the same re-probe cadence.
            prefer_wss: matches!(initial, Some(PreferredTransport::Wss)),
            attempts_since_demotion: 0,
        }
    }

    /// Decide what THIS connect attempt tries. Counts re-probe cadence.
    pub(crate) fn plan(&mut self) -> ConnectPlan {
        match self.policy {
            TransportPolicy::WssOnly => ConnectPlan::Wss,
            TransportPolicy::QuicOnly => ConnectPlan::Quic,
            TransportPolicy::QuicFirst { .. } => {
                if !self.quic_configured {
                    return ConnectPlan::Wss;
                }
                if !self.prefer_wss {
                    return ConnectPlan::QuicThenWss;
                }
                self.attempts_since_demotion += 1;
                if self
                    .attempts_since_demotion
                    .is_multiple_of(QUIC_REPROBE_PERIOD)
                {
                    ConnectPlan::QuicThenWss // opportunistic re-probe
                } else {
                    ConnectPlan::Wss
                }
            }
        }
    }

    /// A QUIC connect succeeded: QUIC is healthy again.
    pub(crate) fn note_quic_success(&mut self) {
        self.consecutive_quic_failures = 0;
        self.prefer_wss = false;
        self.attempts_since_demotion = 0;
    }

    /// A QUIC connect failed (error or timeout).
    pub(crate) fn note_quic_failure(&mut self) {
        self.consecutive_quic_failures += 1;
        if !self.prefer_wss && self.consecutive_quic_failures >= QUIC_FAILURES_BEFORE_WSS {
            self.prefer_wss = true;
            self.attempts_since_demotion = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quic_first() -> TransportPolicy {
        TransportPolicy::default()
    }

    #[test]
    fn default_policy_is_quic_first_with_three_seconds() {
        assert_eq!(
            quic_first(),
            TransportPolicy::QuicFirst {
                quic_timeout: Duration::from_secs(3)
            }
        );
    }

    #[test]
    fn preferred_transport_names_round_trip() {
        for kind in [PreferredTransport::Quic, PreferredTransport::Wss] {
            assert_eq!(PreferredTransport::from_name(kind.as_str()), Some(kind));
        }
        assert_eq!(PreferredTransport::from_name("carrier-pigeon"), None);
        assert_eq!(
            PreferredTransport::from(TransportKind::Quic).as_str(),
            "quic"
        );
        assert_eq!(PreferredTransport::from(TransportKind::Wss).as_str(), "wss");
    }

    #[test]
    fn fixed_policies_never_vary() {
        let mut wss = TransportSelector::new(TransportPolicy::WssOnly, true, None);
        let mut quic = TransportSelector::new(TransportPolicy::QuicOnly, true, None);
        for _ in 0..10 {
            assert_eq!(wss.plan(), ConnectPlan::Wss);
            assert_eq!(quic.plan(), ConnectPlan::Quic);
            wss.note_quic_failure(); // must not influence a fixed policy
            quic.note_quic_failure();
        }
    }

    #[test]
    fn quic_first_without_an_endpoint_is_wss() {
        let mut sel = TransportSelector::new(quic_first(), false, None);
        for _ in 0..5 {
            assert_eq!(sel.plan(), ConnectPlan::Wss);
        }
    }

    #[test]
    fn two_consecutive_failures_demote_to_wss_with_reprobes() {
        let mut sel = TransportSelector::new(quic_first(), true, None);
        assert_eq!(sel.plan(), ConnectPlan::QuicThenWss);
        sel.note_quic_failure();
        // One failure is not enough: the next attempt still leads with QUIC.
        assert_eq!(sel.plan(), ConnectPlan::QuicThenWss);
        sel.note_quic_failure();
        // Two consecutive failures: WSS preferred, QUIC re-probed on every
        // 4th attempt thereafter.
        let plans: Vec<ConnectPlan> = (0..8).map(|_| sel.plan()).collect();
        assert_eq!(
            plans,
            vec![
                ConnectPlan::Wss,
                ConnectPlan::Wss,
                ConnectPlan::Wss,
                ConnectPlan::QuicThenWss, // 4th: re-probe
                ConnectPlan::Wss,
                ConnectPlan::Wss,
                ConnectPlan::Wss,
                ConnectPlan::QuicThenWss, // 8th: re-probe again
            ]
        );
    }

    #[test]
    fn a_success_between_failures_resets_the_count() {
        let mut sel = TransportSelector::new(quic_first(), true, None);
        sel.note_quic_failure();
        sel.note_quic_success();
        sel.note_quic_failure();
        // Never two CONSECUTIVE failures: QUIC stays first.
        assert_eq!(sel.plan(), ConnectPlan::QuicThenWss);
    }

    #[test]
    fn reprobe_success_restores_quic_preference() {
        let mut sel = TransportSelector::new(quic_first(), true, None);
        sel.note_quic_failure();
        sel.note_quic_failure();
        for _ in 0..3 {
            assert_eq!(sel.plan(), ConnectPlan::Wss);
        }
        assert_eq!(sel.plan(), ConnectPlan::QuicThenWss, "re-probe slot");
        sel.note_quic_success();
        // Healthy again: every attempt leads with QUIC.
        for _ in 0..6 {
            assert_eq!(sel.plan(), ConnectPlan::QuicThenWss);
        }
    }

    #[test]
    fn failed_reprobe_keeps_wss_preference_and_cadence() {
        let mut sel = TransportSelector::new(quic_first(), true, None);
        sel.note_quic_failure();
        sel.note_quic_failure();
        for _ in 0..3 {
            assert_eq!(sel.plan(), ConnectPlan::Wss);
        }
        assert_eq!(sel.plan(), ConnectPlan::QuicThenWss);
        sel.note_quic_failure(); // the re-probe also failed
        let plans: Vec<ConnectPlan> = (0..4).map(|_| sel.plan()).collect();
        assert_eq!(
            plans,
            vec![
                ConnectPlan::Wss,
                ConnectPlan::Wss,
                ConnectPlan::Wss,
                ConnectPlan::QuicThenWss,
            ]
        );
    }

    #[test]
    fn wss_initial_preference_starts_demoted_but_still_reprobes() {
        let mut sel = TransportSelector::new(quic_first(), true, Some(PreferredTransport::Wss));
        let plans: Vec<ConnectPlan> = (0..4).map(|_| sel.plan()).collect();
        assert_eq!(
            plans,
            vec![
                ConnectPlan::Wss,
                ConnectPlan::Wss,
                ConnectPlan::Wss,
                ConnectPlan::QuicThenWss,
            ]
        );
    }

    #[test]
    fn quic_initial_preference_is_the_normal_path() {
        let mut sel = TransportSelector::new(quic_first(), true, Some(PreferredTransport::Quic));
        assert_eq!(sel.plan(), ConnectPlan::QuicThenWss);
    }
}
