# epics-ca-rs

Pure Rust implementation of the [EPICS Channel Access](https://docs.epics-controls.org/en/latest/internal/ca_protocol.html) protocol — client, server, and CLI tools.

No C dependencies. No `libca`. Just `cargo build`.

**100% wire-compatible** with C EPICS clients and servers (`caget`, `camonitor`, `caRepeater`, CSS, PyDM, Phoebus).

**Repository:** <https://github.com/epics-rs/epics-rs>

## Overview

epics-ca-rs implements the full Channel Access protocol used by C EPICS for over 30 years. Both client and server share the same protocol module, ensuring symmetric encoding. The wire format is byte-for-byte identical to C EPICS, so you can mix Rust and C IOCs/clients freely on the same network.

```
┌──────────────┐                         ┌──────────────┐
│  CA Client   │ ─── UDP search ───────► │  CA Server   │
│  (caget-rs)  │ ◄── search response ─── │  (softioc-rs)│
│              │                         │              │
│              │ ─── TCP virt circuit ─► │              │
│              │ ◄── DBR data ────────── │              │
└──────────────┘                         └──────────────┘
```

## Features

### Protocol
- **CA header** — standard 16-byte header with command/payload_size/data_type/data_count/parameter1/parameter2
- **Extended header** — 32-byte form for payloads >64 KB or counts >65535
- **Big-endian wire format** — matches C EPICS exactly
- **All commands** — VERSION, EVENT_ADD, EVENT_CANCEL, READ_NOTIFY, WRITE_NOTIFY, SEARCH, NOT_FOUND, ACCESS_RIGHTS, RSRV_IS_UP, BEACON, etc.
- **DBR type encoding** — PLAIN(0-6), STS(7-13), TIME(14-20), GR(21-27), CTRL(28-34) for all 7 native types (String, Short, Float, Enum, Char, Long, Double)
- **String padding** — 40-byte fixed strings with null termination
- **GR/CTRL metadata** — units, precision, display limits, control limits, alarm limits

### Server
- **CaServer** — multi-channel TCP server backed by `Arc<PvDatabase>`
- **UDP responder** — search request handling with name resolution against the database
- **TCP virtual circuit** — per-client connection state, request multiplexing
- **Beacon emitter** — periodic RSRV_IS_UP broadcasts (15s interval default), reset on connect/disconnect for fast client recovery
- **Monitor subscriptions** — `EVENT_ADD` with deadband filtering (MDEL/ADEL via Snapshot deadband logic), DBE_VALUE/DBE_LOG/DBE_ALARM masks
- **Access security** — per-channel READ/WRITE permission via ACF rules
- **Origin tracking** — self-write loop prevention for sequencer-style applications
- **Compatible with**: caget, camonitor, cainfo, caput, EPICS shell tools, CSS, PyDM, Phoebus, PyEpics, caproto

### Client
- **CaClient** — `caget`, `caput`, `camonitor` API
- **CaChannel** — connection state machine: searching → connected → monitoring
- **UDP search** — broadcast to `EPICS_CA_ADDR_LIST`, fallback to `EPICS_CA_AUTO_ADDR_LIST`
- **Beacon monitor** — passive search trigger when servers (re)announce
- **Subscription** — async stream of monitor events with deadband filtering
- **Reconnect** — automatic recovery on server restart (beacon-driven)

### Beyond the C protocol

Subsystems with no C counterpart, each behind its own feature so a build that
does not want one does not compile it:

- **Service discovery** (`discovery`) — mDNS + DNS-SD announcement and
  lookup, and RFC 2136 dynamic DNS registration with TSIG signing
  (`discovery-dns-update`). An IOC can register itself in site DNS instead of
  relying on broadcast search. See `doc/12-discovery.md`.
- **CA over TLS** (`experimental-rust-tls`) — NOT part of the EPICS spec:
  rustls on the virtual circuit, with client certificates, an SNI map and a
  handshake timeout. See `doc/11-tls-design.md`.
- **Capability tokens** (`cap-tokens`) — Ed25519-signed client identity for
  clients behind NAT or in containers, where host/user matching cannot
  identify them.
- **Observability** (`observability`, `otlp`) — a Prometheus exporter and an
  OTLP trace exporter over the bare `metrics`/`tracing` facades the crate
  always emits on, plus an on-disk event recorder and replay
  (`doc/10-observability.md`).
- **Server hardening** — a structured audit log, per-client rate limiting with
  strikes, signed beacons, an introspection HTTP endpoint, and a drain grace
  period for shutdown.
- **`calink`** — the `ca://` record-link resolver, which is what the
  `client-core` feature exists for: an RTEMS build takes the client a record
  link needs without the UDP discovery stack.

### CLI Tools
- **caget-rs** — read PV value (single shot)
- **caput-rs** — write PV value
- **camonitor-rs** — subscribe to PV changes
- **cainfo-rs** — display PV metadata (host, access, type, count, etc.)
- **ca-repeater-rs** — CA repeater daemon (UDP search forwarding)
- **softioc-rs** — soft IOC server (driven by CLI args, .db files, or st.cmd)
- **ca-lint-rs** — static linter for a CA deployment's configuration
- **ca-admin-rs** — query a running server's introspection endpoint
- **ca-replay-rs** — replay a recorded CA event log
- **ca-soak**, **ca-soak-observed** — long-running client soak, the second
  wired to the observability stack
- **realtime-ca-ioc** — the RTEMS CA IOC entry point

## Feature levers

| feature | what it adds |
|---|---|
| `client` (default) | the full client: `client-core` plus the host-only UDP discovery stack — beacon monitor, repeater registration, `discovery`, reverse-DNS peer names |
| `client-core` | the client a record link needs and nothing else — search, virtual circuit, subscriptions, the `ca://` resolver. The configuration an RTEMS target builds |
| `discovery` | mDNS + DNS-SD announcement and lookup |
| `discovery-dns-update` | RFC 2136 dynamic DNS registration, TSIG-signed |
| `experimental-rust-tls` | CA over TLS (not an EPICS protocol feature) |
| `cap-tokens` | Ed25519 capability-token identity |
| `observability` | bundled Prometheus exporter + `tracing-subscriber` init |
| `otlp` | OpenTelemetry OTLP trace export |
| `bringup-probes` | the RTEMS bring-up measurement rig in `realtime-ca-ioc` |

## Architecture

```
epics-ca-rs/src/
├── lib.rs
├── protocol.rs             # CA header (standard + extended), command codes
├── channel.rs              # CA channel state model
├── iocinf.rs               # the address-list env-var helpers (libca iocinf.cpp)
├── estdlib.rs              # C parsing semantics for the numeric knobs
├── copt.rs                 # C option-argument semantics for the CLI tools
├── hostname.rs             # peer address → the text libca shows for it
├── client/
│   ├── mod.rs              # CaClient
│   ├── transport.rs        # TCP virtual circuit
│   ├── search.rs           # UDP search broadcaster
│   ├── beacon_monitor.rs   # passive beacon listener
│   ├── subscription.rs     # async monitor stream
│   ├── sync_group.rs       # SyncGroup — batched ops + collective wait
│   ├── circuit_breaker.rs  # per-server backoff
│   ├── state.rs            # connection state machine
│   └── types.rs            # CaError, CaValue
├── server/
│   ├── mod.rs              # re-exports
│   ├── ca_server.rs        # CaServer (top-level)
│   ├── ioc_app.rs          # adapter for IocApplication::run
│   ├── iocsh.rs            # the server's iocsh command (`casr`)
│   ├── tcp.rs, recv.rs, send.rs, frame.rs, outbox.rs
│   │                       # the virtual circuit, split by direction
│   ├── udp.rs              # UDP search responder
│   ├── addr_list.rs        # EPICS_CAS_* interface/beacon address lists
│   ├── beacon.rs           # RSRV_IS_UP emitter
│   ├── signed_beacon.rs    # optional beacon signing
│   ├── monitor.rs          # subscription handling
│   ├── access_token.rs     # the type-state ACF gate on every channel
│   ├── rate_limit.rs       # per-client message and search rate limits
│   ├── blocking.rs         # the blocking-front-end server path
│   ├── introspection.rs    # the admin HTTP endpoint
│   └── stats.rs            # the counters casr and Prometheus both read
├── calink/                 # `ca://` record links (resolver + iocsh)
├── discovery/              # mDNS, DNS-SD, DNS UPDATE, TSIG, zone files
├── tls/                    # CA over TLS (experimental)
├── cap_token.rs            # Ed25519 capability tokens
├── audit.rs                # structured audit log
├── observability.rs        # metrics/tracing wiring
├── replay.rs               # event-log record and replay
├── chaos.rs                # fault injection for tests
├── repeater.rs             # CA repeater daemon
├── repeater_clients.rs     # the repeater's client registry
└── bin/                    # the twelve CLI tools listed above
```

## Quick Start

### CLI

```bash
# Start a soft IOC with two PVs
softioc-rs --pv TEMP:double:25.0 --pv MSG:string:hello

# Read, write, monitor (in another terminal)
caget-rs TEMP
caput-rs TEMP 30.5
camonitor-rs TEMP

# C EPICS tools work too
caget TEMP
camonitor TEMP
```

### Server (programmatic)

```rust
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::records::ai::AiRecord;
use epics_ca_rs::server::run_ca_ioc_app;

#[epics_base_rs::epics_main]
async fn main() -> epics_base_rs::error::CaResult<()> {
    // `run_ca_ioc_app` runs C's `rsrvRegistrar` before the startup script,
    // which `IocApplication::run(run_ca_ioc)` cannot: it is dispatched after.
    run_ca_ioc_app(IocApplication::new().record("TEMP", AiRecord::new())).await
}
```

### Client

```rust
use epics_ca_rs::client::CaClient;

let client = CaClient::new().await?;
let (dbf_type, value) = client.caget("TEMP").await?;
client.caput("TEMP", "42.0").await?;

let mut sub = client.camonitor("TEMP").await?;
while let Some(event) = sub.recv().await {
    println!("{:?}", event.value);
}
```

## Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `EPICS_CA_ADDR_LIST` | (empty) | Comma-separated list of CA server addresses |
| `EPICS_CA_AUTO_ADDR_LIST` | `YES` | Append broadcast addresses of all interfaces |
| `EPICS_CA_SERVER_PORT` | `5064` | Server TCP/UDP port |
| `EPICS_CA_REPEATER_PORT` | `5065` | Repeater UDP port |
| `EPICS_CA_MAX_ARRAY_BYTES` | `16384` | Maximum array transfer size |
| `EPICS_CA_BEACON_PERIOD` | `15` | Server beacon interval (seconds) |
| `EPICS_CA_NAME_SERVERS` | (empty) | Unicast search targets — the only search a target with no UDP broadcast has |

The server-side `EPICS_CAS_*` set (interface and beacon address lists,
channel and subscription limits, timeouts, rate limits, TLS, audit) and the
rest of the client set are tabulated in `doc/08-environment.md`.

## Testing

```bash
cargo test -p epics-ca-rs
```

Test coverage: header encode/decode (golden packets vs `caget`), DBR encoding for all type ranges, big-endian conversion, beacon timing, search request/response, virtual circuit handshake, monitor deadband filtering, extended header (>64 KB), origin tracking, multi-channel session.

## Dependencies

- epics-base-rs — PvDatabase, records, EpicsValue, DBR codec, and the
  `runtime`/`net` layer (this crate does not depend on tokio directly)
- chrono, thiserror, parking_lot, arc-swap, dashmap — the always-on set
- clap — the CLI tools
- metrics, tracing — the facades the crate emits on with no exporter attached

Everything else is feature-gated: mdns-sd and hickory-* for `discovery`,
tokio-rustls and x509-parser for `experimental-rust-tls`, ed25519-dalek for
`cap-tokens`, the opentelemetry tree for `otlp`.

## Requirements

- Rust 1.94.0 (`rust-toolchain.toml`), edition 2024

## License

[EPICS Open License](../../LICENSE)
