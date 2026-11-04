# 10 — Observability: tracing and metrics

`epics-ca-rs` emits structured `tracing` events and `metrics` (the
facade crate) counters/gauges/histograms throughout the CA stack. By
default both are zero-overhead — they only do work when a subscriber
is installed. Consumers either:

1. Bring their own `tracing-subscriber` and `metrics` recorder (full
   control).
2. Enable the `observability` cargo feature for a bundled
   tracing-subscriber + Prometheus exporter helper.

## Structured tracing

All emit sites use the standard `tracing` macros. Common fields:

| Field | Type | Where |
|-------|------|-------|
| `pv` | string | per-channel events (connect, disconnect) |
| `cid` | u32 | client channel id |
| `sid` | u32 | server-assigned id (post-connect only) |
| `server` | SocketAddr | TCP virtual circuit identity |
| `peer` | SocketAddr | server-side client identity |
| `subid` | u32 | subscription id |
| `ioid` | u32 | read/write request id |

### Event severity guide

| Level | When |
|-------|------|
| `error` | logic error or unrecoverable condition (currently rare; we prefer `warn` + cleanup) |
| `warn` | unexpected but recoverable: TCP closed, echo timeout, beacon anomaly, monitor dropped, connect failure, hostname spoof attempt |
| `info` | major lifecycle: channel connected/reconnected, client accepted/disconnected, exporter started |
| `debug` | per-message protocol activity: PV search resolved, TCP circuit established |
| `trace` | byte-level (currently unused in production paths) |

### Example output

```
INFO  channel connected pv=MOTOR:X:VAL cid=1 sid=7 server=10.0.0.5:5064
DEBUG PV search resolved pv=BPM:01:X cid=2 server=10.0.0.5:5064
WARN  monitor dropped (consumer queue full) subid=42
WARN  beacon anomaly detected — IOC may have restarted server=10.0.0.5:5064
INFO  channel reconnected pv=MOTOR:X:VAL cid=1 sid=8 server=10.0.0.5:5064
```

## Metrics

### Client-side metrics

| Name | Type | Labels | Meaning |
|------|------|--------|---------|
| `ca_client_connections_total` | counter | `server` | Successful CREATE_CHAN responses |
| `ca_client_disconnections_total` | counter | `server` | TcpClosed / SERVER_DISCONN events |
| `ca_client_tcp_closed_total` | counter | `server` | Distinct TCP circuit terminations |
| `ca_client_unresponsive_total` | counter | `server` | First echo timeout (1st miss) |
| `ca_client_beacon_anomalies_total` | counter | `server` | beacon ID skip / fast period |
| `ca_client_search_responses_total` | counter | (none) | Total SEARCH replies received |
| `ca_client_search_rtt_seconds` | histogram | `server` | UDP search RTT, per destination |
| `ca_client_channels_connected` | gauge | (none) | Currently-connected channels |

### Server-side metrics

| Name | Type | Meaning |
|------|------|---------|
| `ca_server_accepts_total` | counter | TCP accept events |
| `ca_server_disconnects_total` | counter | Per-client task exits |
| `ca_server_clients_active` | gauge | Currently connected clients |

More server-side metrics can be added as the stability work continues
(per-PV monitor backlog, channel count per client, etc.).

## The `observability` feature

When you enable it:

```toml
[dependencies]
epics-ca-rs = { version = "...", features = ["observability"] }
```

You get two convenience helpers:

```rust
use epics_ca_rs::observability;

// Wires a tracing-subscriber that reads RUST_LOG (defaults to
// "info,epics_ca_rs=debug") and writes to stderr.
observability::init_tracing();

// Installs the global metrics recorder + spins up a Prometheus
// exporter on the given address. Drop guard is held internally.
observability::serve_prometheus("0.0.0.0:9090".parse()?)?;
```

After that, `curl http://0.0.0.0:9090/metrics` yields the standard
text exposition format consumable by Prometheus, Grafana Cloud,
VictoriaMetrics, etc.

## Bringing your own stack

When the `observability` feature is **off** (default), `epics-ca-rs`
only depends on the bare `tracing = "0.1"` and `metrics = "0.23"`
facade crates. You install whichever subscriber/recorder fits your
infrastructure:

```rust
// Custom subscriber
tracing_subscriber::fmt()
    .with_env_filter("info,epics_ca_rs=trace")
    .with_target(true)
    .json()                            // JSON for log shipping
    .init();

// Custom metrics recorder
let recorder = my_metrics_backend::Recorder::new();
metrics::set_global_recorder(Box::new(recorder)).unwrap();
```

You can mix freely — e.g. JSON tracing to ELK + StatsD metrics to
Datadog — without the bundled feature.

## Sample integrations

### Jaeger / OpenTelemetry

```rust
use opentelemetry::sdk::Resource;
use opentelemetry_otlp::WithExportConfig;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::prelude::*;

let tracer = opentelemetry_otlp::new_pipeline()
    .tracing()
    .with_exporter(opentelemetry_otlp::new_exporter().tonic())
    .with_trace_config(
        opentelemetry::sdk::trace::config()
            .with_resource(Resource::new([opentelemetry::KeyValue::new(
                "service.name", "my-ca-app")]))
    )
    .install_batch(opentelemetry::runtime::Tokio)?;

tracing_subscriber::registry()
    .with(tracing_subscriber::EnvFilter::from_default_env())
    .with(OpenTelemetryLayer::new(tracer))
    .init();
```

Now every CA event becomes a span in your distributed-tracing system.

### Grafana Cloud

```rust
metrics_exporter_prometheus::PrometheusBuilder::new()
    .with_push_gateway("https://prometheus-blocks-prod-us-central1.grafana.net/api/prom/push",
                      Duration::from_secs(15), "user", "api-key")
    .install()?;
```

### Console-only debug

```bash
RUST_LOG=info,epics_ca_rs=trace cargo run
```

## Performance overhead

- Tracing: roughly 20 ns per disabled span (atomic load to check enabled
  state). Per emitted event, depends on subscriber — `fmt::layer` is
  ~2 µs, JSON serializer is ~10 µs, OpenTelemetry batch is amortized.
- Metrics: counter/gauge updates are atomic ops (~5 ns). Histograms
  with quantile estimation are ~50 ns each.

The hot paths (per-monitor delivery, per-search packet) emit at
`debug`/`trace` level so they're free unless explicitly enabled.

## Diagnosing common operational issues

| Symptom | Metric / log to check |
|---------|------------------------|
| Channels keep flapping | `ca_client_disconnections_total` rate, log `channel reconnected` events |
| Monitor latency growing | `ca_client_unresponsive_total` |
| Slow PV search | `ca_client_search_rtt_seconds` p99 |
| Server overloaded | `ca_server_clients_active` gauge, `ca_server_accepts_total` rate |
| IOC unstable | `ca_client_beacon_anomalies_total` per server |

## OTLP trace export

Build with the `otlp` feature to ship spans to an OpenTelemetry
collector (Tempo, Jaeger, OTel Collector) over gRPC:

```rust
epics_ca_rs::observability::init_otlp(
    "http://otel-collector.facility.local:4317",
    "softioc-motor-1",
)?;
```

Or rely on the standard OTel env vars:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT="http://otel-collector:4317"
export OTEL_SERVICE_NAME="softioc-motor-1"
```

…and call `init_otlp_from_env()`. Returns `Ok(false)` when the
endpoint is unset so callers can fall back to plain `init_tracing`.

The exporter installs a global tracer provider; flush buffered spans
on process exit with
`opentelemetry::global::shutdown_tracer_provider()`.

The dependency tree (`opentelemetry`, `opentelemetry-otlp`,
`tracing-opentelemetry`) is gated behind the feature — default builds
carry zero OTLP code.

## See also

- [`07-flow-control.md`](07-flow-control.md) — what's measured by
  dropped-monitor and unresponsive counters
- [`05-state-machines.md`](05-state-machines.md) — the lifecycle
  transitions that emit `connection_events`
- [`../TESTING.md`](../TESTING.md) — `ca-soak-observed` binary that
  wires everything together for live demos
