# 10 — Observability

`pva-rs` emits two kinds of runtime signal: structured `tracing`
events and Prometheus-style metrics counters. Binaries opt-in via
the standard subscriber wiring; library users can plug into either.

## tracing

Targets follow the module path:

| Target | Typical events |
|--------|----------------|
| `epics_pva_rs::client_native::server_conn` | connection up/down, heartbeat tick, idle timeout |
| `epics_pva_rs::client_native::search_engine` | searches sent, beacons received, GUID changes |
| `epics_pva_rs::client_native::channel` | state transitions (Searching → Active, etc.), holdoff |
| `epics_pva_rs::client_native::ops_v2` | per-op INIT / DATA / DESTROY trace |
| `epics_pva_rs::server_native::tcp` | accept, handshake, op dispatch, watermark crossings |
| `epics_pva_rs::server_native::udp` | search received, beacon emitted |
| `epics_pva_rs::server_native::shared_pv` | put / post / lifecycle hooks |
| `epics_pva_rs::auth` | TLS handshake, cap-token verify (when enabled) |

Levels:

- `error` — protocol violations from peers; uncatchable internal errors.
- `warn` — recoverable failures (idle disconnect, monitor lagged, holdoff).
- `info` — connection lifecycle, server start/stop, ACF reload.
- `debug` — per-op tracing.
- `trace` — every wire frame dump (verbose, off by default).

### `RUST_LOG` examples

```text
RUST_LOG=info                         # default for daemons
RUST_LOG=epics_pva_rs=debug,info      # debug pva-rs, info elsewhere
RUST_LOG=epics_pva_rs::client_native::ops_v2=trace
PVXS_LOG=epics_pva_rs=debug           # read directly by crate::log::init_filter (pvxs PVXS_LOG parity); takes precedence over EPICS_PVA_LOG and RUST_LOG
```

### Reload

Set up a reload-able subscriber via `crate::log::init_filter`. From
running code, call `crate::log::set_log_filter(spec)` or
`crate::log::set_log_level(target, level)` to change the filter
without restarting the process. Mirrors pvxs `logger_config_str` /
`logger_level_set`.

## metrics

We use the `metrics` crate facade. Connect any `metrics-exporter-*`
implementation in your binary. The schema:

### Server side (`epics_pva_rs::server_native`)

| Counter / gauge | Labels | Where |
|-----------------|--------|-------|
| `pva_server_accepts_total` | — | `tcp.rs::run_tcp_server` |
| `pva_server_disconnects_total` | — | `tcp.rs::handle_connection_io` |
| `pva_server_clients_active` (gauge) | — | accept / disconnect |
| `pva_server_channels_active` (gauge) | — | CREATE_CHANNEL / DestroyChannel |
| `pva_server_ops_total` | `kind=get|put|monitor|rpc` | `handle_op` |
| `pva_server_monitor_overflow_total` | `pv` | `tcp.rs` monitor fan-out |
| `pva_server_search_received_total` | — | `udp.rs::run_udp_responder_with_config` |
| `pva_server_beacon_emitted_total` | — | beacon emitter |

### Client side (`epics_pva_rs::client_native`)

| Counter / gauge | Labels | Where |
|-----------------|--------|-------|
| `pva_client_searches_sent_total` | — | `search_engine.rs::send_due_searches` |
| `pva_client_search_responses_total` | — | UDP recv path |
| `pva_client_beacons_received_total` | `guid_known=true|false` | beacon listener |
| `pva_client_connections_total` | — | `server_conn.rs::run_handshake_and_spawn` |
| `pva_client_disconnects_total` | — | reader / writer task exit |
| `pva_client_idle_timeouts_total` | — | heartbeat path |
| `pva_client_ops_total` | `kind=get|put|monitor|rpc` | `ops_v2.rs` |
| `pva_client_monitor_lagged_total` | `pv` | broadcast lagged path |

## Server health snapshot

`PvaServer::report()` returns `ServerReport`:

```rust
pub struct ServerReport {
    pub tcp_port: u16,
    pub udp_port: u16,
    pub tls_enabled: bool,
    pub ignore_addrs: usize,
    pub beacon_period_secs: u64,
    pub udp_alive: bool,
    pub tcp_alive: bool,
}
```

Useful for ad-hoc liveness checks (`udp_alive && tcp_alive` ⇒
"serving") and for binaries that print "listening on tcp/X udp/Y"
on startup.

## Audit logging

`PvaServerConfig::auth_complete` is the closest thing to a
structured audit hook today: invoked once per accepted connection
after CONNECTION_VALIDATION. Bind it to record `(peer, method,
account, host, roles)` into your audit pipeline.
