//! `dice-logging` — tracing initialization for Dice services.
//!
//! Thin by design (see `docs/design/workspace-and-protocol.md` §5.7): every
//! service binary calls [`LogConfig::from_env`] + [`init`] exactly once at
//! startup. The filter comes from `RUST_LOG` (falling back to
//! [`DEFAULT_FILTER`]) and the output format is human-readable by default,
//! switching to newline-delimited JSON when `DICE_LOG_JSON=1`.
//!
//! When `DICE_OTLP_ENDPOINT` is set (e.g. `http://localhost:4318`), [`init`]
//! also installs an OpenTelemetry layer that exports spans over OTLP/HTTP and
//! a W3C `traceparent` propagator — so a request's trace context can cross the
//! split-mode NATS-RPC boundary (see `dice_event_bus::rpc`) and show up as one
//! end-to-end trace. Export is OFF by default, so dev-lite and tests are
//! unaffected. Call [`shutdown`] before exit to flush pending spans.
//!
//! [`init`] uses `try_init` internally, so calling it twice (e.g. from
//! multiple `#[test]`s in one process) never panics — the second call is a
//! no-op that returns `false`.

use std::sync::OnceLock;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

/// Filter applied when `RUST_LOG` is unset or empty.
pub const DEFAULT_FILTER: &str = "info,dice=debug";

/// Holds the exporting tracer provider so [`shutdown`] can flush it on exit.
static TRACER_PROVIDER: OnceLock<opentelemetry_sdk::trace::TracerProvider> = OnceLock::new();

/// Output format for log events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable single-line output with ANSI colors (dev default).
    Pretty,
    /// Newline-delimited JSON, one event per line (production / log shippers).
    Json,
}

/// Logging configuration, normally built via [`LogConfig::from_env`].
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// A `tracing_subscriber::EnvFilter` directive string,
    /// e.g. `"info,dice=debug"`.
    pub filter: String,
    /// Output format.
    pub format: LogFormat,
    /// OTLP/HTTP traces endpoint (`DICE_OTLP_ENDPOINT`, e.g.
    /// `http://localhost:4318`). `None` disables span export — the default.
    pub otlp_endpoint: Option<String>,
    /// `service.name` resource for exported traces (`DICE_SERVICE_NAME`,
    /// default `"dice"`).
    pub service_name: String,
}

impl LogConfig {
    /// Builds the configuration from the environment:
    ///
    /// - `filter`: `RUST_LOG` if set and non-empty, else [`DEFAULT_FILTER`].
    /// - `format`: [`LogFormat::Json`] when `DICE_LOG_JSON=1`, else
    ///   [`LogFormat::Pretty`].
    /// - `otlp_endpoint`: `DICE_OTLP_ENDPOINT` if set and non-empty, else none.
    /// - `service_name`: `DICE_SERVICE_NAME` if set, else `"dice"`.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            filter: filter_from(std::env::var("RUST_LOG").ok()),
            format: format_from(std::env::var("DICE_LOG_JSON").ok()),
            otlp_endpoint: std::env::var("DICE_OTLP_ENDPOINT")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            service_name: std::env::var("DICE_SERVICE_NAME")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "dice".to_owned()),
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            filter: DEFAULT_FILTER.to_owned(),
            format: LogFormat::Pretty,
            otlp_endpoint: None,
            service_name: "dice".to_owned(),
        }
    }
}

fn filter_from(rust_log: Option<String>) -> String {
    match rust_log {
        Some(s) if !s.trim().is_empty() => s,
        _ => DEFAULT_FILTER.to_owned(),
    }
}

fn format_from(dice_log_json: Option<String>) -> LogFormat {
    if dice_log_json.as_deref() == Some("1") {
        LogFormat::Json
    } else {
        LogFormat::Pretty
    }
}

/// Installs the global tracing subscriber described by `cfg`.
///
/// Returns `true` if this call installed the subscriber, `false` if a global
/// subscriber was already set (the call is then a no-op — it never panics, so
/// repeated initialization in tests is safe).
///
/// An invalid `cfg.filter` directive falls back to [`DEFAULT_FILTER`] with a
/// warning on stderr rather than failing startup. An OTLP exporter that fails
/// to build is logged and skipped — logging still comes up.
pub fn init(cfg: &LogConfig) -> bool {
    let filter = EnvFilter::try_new(&cfg.filter).unwrap_or_else(|err| {
        eprintln!(
            "dice-logging: invalid filter directive {:?} ({err}); falling back to {:?}",
            cfg.filter, DEFAULT_FILTER
        );
        EnvFilter::new(DEFAULT_FILTER)
    });

    let fmt_layer = match cfg.format {
        LogFormat::Pretty => tracing_subscriber::fmt::layer().boxed(),
        LogFormat::Json => tracing_subscriber::fmt::layer().json().boxed(),
    };

    // Optional OTLP span export. `Layer` is implemented for `Option<L>`, so a
    // `None` here adds nothing.
    let otel_layer = cfg.otlp_endpoint.as_ref().and_then(|endpoint| {
        match build_otlp_tracer(endpoint, &cfg.service_name) {
            Ok(tracer) => {
                opentelemetry::global::set_text_map_propagator(
                    opentelemetry_sdk::propagation::TraceContextPropagator::new(),
                );
                Some(tracing_opentelemetry::layer().with_tracer(tracer).boxed())
            }
            Err(error) => {
                eprintln!("dice-logging: OTLP export disabled ({error})");
                None
            }
        }
    });

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer)
        .try_init()
        .is_ok()
}

/// Build an OTLP/HTTP batch-exporting tracer and register it as the global
/// provider. Must be called inside a Tokio runtime (the batch exporter spawns
/// on it) — every service `#[tokio::main]` satisfies this.
fn build_otlp_tracer(
    endpoint: &str,
    service_name: &str,
) -> Result<opentelemetry_sdk::trace::Tracer, opentelemetry::trace::TraceError> {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig as _;

    // Unlike the OTEL_* env vars, `.with_endpoint` is used verbatim (it does
    // NOT append the signal path), so ensure the OTLP/HTTP traces path is there.
    let base = endpoint.trim_end_matches('/');
    let traces_url = if base.ends_with("/v1/traces") {
        base.to_owned()
    } else {
        format!("{base}/v1/traces")
    };
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(traces_url)
        .build()?;

    let provider = opentelemetry_sdk::trace::TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(opentelemetry_sdk::Resource::new(vec![
            opentelemetry::KeyValue::new("service.name", service_name.to_owned()),
        ]))
        .build();

    let tracer = provider.tracer("dice");
    opentelemetry::global::set_tracer_provider(provider.clone());
    let _ = TRACER_PROVIDER.set(provider);
    Ok(tracer)
}

/// Flush and shut down the OTLP exporter (if any). Best-effort; call once
/// before the process exits so buffered spans are sent. A no-op when export
/// was never enabled.
pub fn shutdown() {
    if let Some(provider) = TRACER_PROVIDER.get() {
        let _ = provider.shutdown();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // The only test that calls `init` (a process-global side effect) so the
    // assertions stay deterministic under parallel test execution.
    #[test]
    fn init_twice_does_not_panic() {
        let cfg = LogConfig::default();
        let first = init(&cfg);
        let second = init(&cfg);
        assert!(first, "first init should install the subscriber");
        assert!(!second, "second init must be a no-op, not a panic");
        // Emitting through the installed subscriber must not panic either.
        tracing::info!("dice-logging init smoke test");

        // An invalid filter directive must fall back, not panic, even when a
        // subscriber is already installed.
        let invalid = LogConfig {
            filter: "not a [valid] directive!!!".to_owned(),
            format: LogFormat::Json,
            ..LogConfig::default()
        };
        assert!(!init(&invalid));
    }

    #[test]
    fn filter_falls_back_to_default() {
        assert_eq!(filter_from(None), DEFAULT_FILTER);
        assert_eq!(filter_from(Some(String::new())), DEFAULT_FILTER);
        assert_eq!(filter_from(Some("   ".to_owned())), DEFAULT_FILTER);
        assert_eq!(filter_from(Some("warn".to_owned())), "warn");
    }

    #[test]
    fn format_selected_by_dice_log_json() {
        assert_eq!(format_from(None), LogFormat::Pretty);
        assert_eq!(format_from(Some("0".to_owned())), LogFormat::Pretty);
        assert_eq!(format_from(Some("true".to_owned())), LogFormat::Pretty);
        assert_eq!(format_from(Some("1".to_owned())), LogFormat::Json);
    }
}
