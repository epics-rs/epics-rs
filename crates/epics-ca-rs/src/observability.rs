//! Optional helpers for wiring up tracing + metrics observability.
//!
//! Compiled only with the `observability` feature so the default build
//! retains zero overhead when not used.
//!
//! # Quick start
//!
//! ```ignore
//! // In a binary that depends on epics-ca-rs with the `observability` feature:
//! epics_ca_rs::observability::init_tracing();
//! let _exporter = epics_ca_rs::observability::serve_prometheus("0.0.0.0:9090".parse()?)?;
//!
//! let client = epics_ca_rs::client::CaClient::new().await?;
//! // ... your code; metrics + structured logs are emitted automatically.
//! ```
//!
//! # Metric names
//!
//! All metrics emitted by epics-ca-rs are prefixed with `ca_client_*`
//! (client side) or `ca_server_*` (server side).

use std::net::SocketAddr;

#[cfg(feature = "observability")]
use metrics_exporter_prometheus::PrometheusBuilder;

/// Initialize a `tracing` subscriber that reads `RUST_LOG` and writes to
/// stderr. Idempotent — calling twice is harmless. Returns true on first
/// call, false on subsequent calls.
#[cfg(feature = "observability")]
pub fn init_tracing() -> bool {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,epics_ca_rs=debug"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false))
        .try_init()
        .is_ok()
}

/// Install the `metrics` global recorder backed by a Prometheus exporter.
/// Spawns an HTTP listener on `addr` that serves `/metrics`.
///
/// Returns the exporter's drop guard. Drop it to stop the listener.
#[cfg(feature = "observability")]
pub fn serve_prometheus(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    PrometheusBuilder::new()
        .with_http_listener(addr)
        .install()?;
    Ok(())
}

/// Stub when the feature is disabled — returns `false` so callers can
/// log "observability not compiled in" without conditional compilation
/// at the call site.
#[cfg(not(feature = "observability"))]
pub fn init_tracing() -> bool {
    false
}

/// Stub when the feature is disabled.
#[cfg(not(feature = "observability"))]
pub fn serve_prometheus(_addr: SocketAddr) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Err("epics-ca-rs built without `observability` feature".into())
}

/// Initialize a `tracing` subscriber that exports spans to an
/// OpenTelemetry OTLP collector (Tempo / Jaeger / OTel Collector)
/// over gRPC, in addition to the regular stderr fmt layer.
///
/// `endpoint` is the collector address — `http://localhost:4317`
/// for a local OTel Collector, or pull from `OTEL_EXPORTER_OTLP_ENDPOINT`.
/// `service_name` lands in the resource attributes so traces are
/// attributable in the backend UI.
///
/// Returns `Ok(())` on success. The exporter installs a global tracer
/// provider; `opentelemetry::global::shutdown_tracer_provider()` on
/// process exit ensures buffered spans flush.
#[cfg(feature = "otlp")]
pub fn init_otlp(
    endpoint: &str,
    service_name: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use opentelemetry::KeyValue;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::Resource;
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .with_endpoint(endpoint);
    let resource = Resource::new(vec![KeyValue::new(
        "service.name",
        service_name.to_string(),
    )]);
    let tracer_provider = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(exporter)
        .with_trace_config(opentelemetry_sdk::trace::Config::default().with_resource(resource))
        .install_batch(opentelemetry_sdk::runtime::Tokio)?;
    let tracer = tracer_provider.tracer("epics-ca-rs");

    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,epics_ca_rs=debug"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false))
        .with(otel_layer)
        .try_init()
        .map_err(|e| format!("tracing init: {e}"))?;
    Ok(())
}

/// Stub when the feature is disabled.
#[cfg(not(feature = "otlp"))]
pub fn init_otlp(
    _endpoint: &str,
    _service_name: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Err("epics-ca-rs built without `otlp` feature".into())
}

/// Resolve OTLP configuration from the environment and initialize.
/// Reads `OTEL_EXPORTER_OTLP_ENDPOINT` (standard OTel env var) and
/// `OTEL_SERVICE_NAME`. Returns `Ok(false)` when no endpoint is
/// configured (so callers can chain into the regular `init_tracing`
/// fallback).
#[cfg(feature = "otlp")]
pub fn init_otlp_from_env() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let endpoint = match std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
        Ok(v) if !v.is_empty() => v,
        _ => return Ok(false),
    };
    let service_name =
        std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "epics-ca-rs".to_string());
    init_otlp(&endpoint, &service_name)?;
    Ok(true)
}

#[cfg(not(feature = "otlp"))]
pub fn init_otlp_from_env() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    Ok(false)
}
