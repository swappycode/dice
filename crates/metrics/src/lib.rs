//! `dice-metrics` — the metrics facade for all Dice services.
//!
//! Services record metrics with the [`counter!`], [`gauge!`], and
//! [`histogram!`] macros re-exported from this crate — **import them from
//! here, never from the `metrics` crate directly**, so the recorder backend
//! stays a single workspace-level decision.
//!
//! # Naming convention (enforced by review)
//!
//! `dice_{service}_{name}_{unit}` — snake_case, with the unit suffix
//! following Prometheus conventions (`_total` for counters, `_seconds`,
//! `_bytes`, ...). Milestone-1 minimum set:
//!
//! - `dice_gateway_connections{transport}`
//! - `dice_gateway_frames_total{dir,class}`
//! - `dice_bus_dropped_events_total`
//! - `dice_chat_messages_total`
//! - `dice_db_pool_acquire_seconds`
//!
//! # Exporter
//!
//! [`init_prometheus`] installs the global Prometheus recorder and serves
//! `GET /metrics` over plain HTTP on the given address. Call it once per
//! process, after logging init. Until it is called, the macros are no-ops —
//! libraries can record unconditionally.

use std::net::SocketAddr;

use metrics_exporter_prometheus::{BuildError, PrometheusBuilder};

pub use metrics::{counter, gauge, histogram};

/// Failure to install the Prometheus exporter.
#[derive(Debug, thiserror::Error)]
pub enum ExporterError {
    /// The underlying builder failed — most commonly because a global
    /// metrics recorder is already installed in this process.
    #[error("failed to install Prometheus exporter: {0}")]
    Install(#[from] BuildError),
}

/// Installs the global Prometheus recorder and starts an HTTP listener
/// serving `GET /metrics` on `bind` (e.g. the `9600` admin port).
///
/// If called inside a Tokio runtime the exporter task is spawned on it;
/// otherwise a dedicated background thread with a single-threaded runtime is
/// created. Note that the TCP socket is bound asynchronously by that task, so
/// an in-use port surfaces as an error log from the exporter task rather than
/// from this function.
///
/// # Errors
///
/// Returns [`ExporterError::Install`] if the recorder could not be installed
/// (e.g. a recorder is already set for this process).
pub fn init_prometheus(bind: SocketAddr) -> Result<(), ExporterError> {
    PrometheusBuilder::new()
        .with_http_listener(bind)
        .install()?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    #[test]
    fn init_prometheus_smoke() {
        // Port 0 = OS-assigned ephemeral port; the listener binds inside the
        // spawned exporter task, so install itself succeeds without
        // colliding with anything on the machine.
        let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        init_prometheus(bind).unwrap();

        // A second install must fail cleanly (recorder already set), not panic.
        assert!(matches!(
            init_prometheus(bind),
            Err(ExporterError::Install(_))
        ));

        // The re-exported macros must record through the installed recorder
        // without panicking.
        counter!("dice_test_events_total").increment(1);
        gauge!("dice_test_inflight_requests").set(1.0);
        histogram!("dice_test_latency_seconds").record(0.001);
    }
}
