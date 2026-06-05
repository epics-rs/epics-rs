# 08 — Environment variables

`pva-rs` reads the same environment variables as pvxs / pvAccessCPP.
This page is a reference: name, default, and where the value is
consulted in the source tree.

## Network

| Variable | Default | Where |
|----------|---------|-------|
| `EPICS_PVA_BROADCAST_PORT` | 5076 | UDP search + beacon port (`client_native/search_engine.rs::DEFAULT_BROADCAST_PORT`, `server_native/runtime.rs::PvaServerConfig::default`) |
| `EPICS_PVA_SERVER_PORT` | 5075 | downstream TCP port (`PvaServerConfig::default`) |
| `EPICS_PVAS_INTF_ADDR_LIST` | `""` (auto) | server bind / multicast join list (`config/env.rs::list_intf_addrs`) |
| `EPICS_PVA_ADDR_LIST` | `""` (auto-broadcast) | client unicast/broadcast targets (`SearchEngine::extra_targets`) |
| `EPICS_PVA_AUTO_ADDR_LIST` | `YES` | when `NO`, suppress per-NIC broadcast fan-out (server beacon + client search) |
| `EPICS_PVA_NAME_SERVERS` | `""` | space-separated `host:port` list for TCP-via-name-server search (`PvaClientBuilder::name_servers`) |
| `EPICS_PVA_BEACON_PERIOD` | 15 (s) | server-side beacon period (`PvaServerConfig::beacon_period`) |
| `EPICS_PVAS_IGNORE_ADDR_LIST` | `""` | server search-ignore list (`PvaServerConfig::ignore_addrs`) |
| `EPICS_PVAS_BEACON_ADDR_LIST` | `""` | explicit beacon destinations (`PvaServerConfig::beacon_destinations`) |

## Auth

| Variable | Default | Where |
|----------|---------|-------|
| `EPICS_PVA_AUTH_METHOD` | `ca` | client preferred method when both offered (`auth/mod.rs`) |
| `EPICS_PVA_TLS_CERT` | unset | client cert path for mTLS (`auth/tls.rs::client_from_env`) |
| `EPICS_PVA_TLS_KEY` | unset | client key path |
| `EPICS_PVA_TLS_CA` | unset | extra trust bundle |
| `EPICS_PVAS_TLS_CERT` | unset | server cert path (`auth/tls.rs::server_from_env`) |
| `EPICS_PVAS_TLS_KEY` | unset | server key path |
| `EPICS_PVAS_TLS_CLIENT_CERT` | `none` / `optional` / `require` | mTLS client-cert policy |
| `EPICS_PVA_TLS_DISABLE` | `NO` | when `YES`, client falls back to plaintext even if cert is configured |

## Tracing / log

| Variable | Default | Where |
|----------|---------|-------|
| `PVXS_LOG` | unset | highest-precedence log filter spec, read directly by `crate::log::init_filter` for pvxs `logger_config_env` parity; applied verbatim as a `tracing_subscriber::EnvFilter` |
| `EPICS_PVA_LOG` | unset | retained Rust-only alias for the filter spec; used only when `PVXS_LOG` is unset |
| `RUST_LOG` | unset | standard `tracing-subscriber` filter; used only when neither `PVXS_LOG` nor `EPICS_PVA_LOG` is set |

## Operational

| Variable | Default | Where |
|----------|---------|-------|
| `EPICS_PVA_CONN_TMO` | 30 (s) | client-side TCP idle timeout (`PvaClientBuilder::tcp_timeout` default) |
| `EPICS_PVA_PIPELINE_SIZE` | 4 | monitor pipeline depth (`ops_v2.rs::DEFAULT_PIPELINE_SIZE`) |
| `EPICS_PVAS_MAX_CONNECTIONS` | 1024 | server hard cap (`PvaServerConfig::max_connections`) |
| `EPICS_PVAS_MAX_CHANNELS_PER_CONN` | 256 | server hard cap (`PvaServerConfig::max_channels_per_connection`) |
| `EPICS_PVAS_OP_TIMEOUT` | 60 (s) | server per-connection idle timeout (`PvaServerConfig::op_timeout`) |
| `EPICS_PVAS_MONITOR_QUEUE_DEPTH` | 4 | server monitor squash threshold (`PvaServerConfig::monitor_queue_depth`) |
| `EPICS_PVAS_MONITOR_HIGH_WATERMARK` | 64 | server monitor backpressure warning threshold |

## Diagnostic / experimental

| Variable | Default | Where |
|----------|---------|-------|
| `EPICS_PVAS_EMIT_TYPE_CACHE` | `NO` | when `YES`, server emits 0xFD/0xFE markers (incompat. with pvAccessCPP — see `07-introspection-cache.md`) |
| `EPICS_PVA_RS_CHAOS` | unset | hidden test knob — when set, server randomly stalls / drops reads (`crate::chaos`) |
| `EPICS_PVA_RS_FORCE_INLINE_TYPE` | `YES` | client preference: when `YES`, never request `OPT_TYPECACHE` from server |

## How variables are consumed

The crate goes through `epics_base_rs::runtime::env::get(name)` to
read every variable, so a process running the test runtime in
isolation (`run_in_isolated_env`) sees its own env without affecting
peers. Production binaries fall through to `std::env::var`.
