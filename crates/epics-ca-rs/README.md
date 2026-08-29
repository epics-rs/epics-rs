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

### CLI Tools
- **caget-rs** — read PV value (single shot)
- **caput-rs** — write PV value
- **camonitor-rs** — subscribe to PV changes
- **cainfo-rs** — display PV metadata (host, access, type, count, etc.)
- **ca-repeater-rs** — CA repeater daemon (UDP search forwarding)
- **softioc-rs** — soft IOC server (driven by CLI args, .db files, or st.cmd)

## Architecture

```
epics-ca-rs/src/
├── lib.rs
├── protocol.rs             # CA header (standard + extended), command codes
├── channel.rs              # CA channel state model
├── client/
│   ├── mod.rs              # CaClient
│   ├── transport.rs        # TCP virtual circuit
│   ├── search.rs           # UDP search broadcaster
│   ├── beacon_monitor.rs   # passive beacon listener
│   ├── subscription.rs     # async monitor stream
│   ├── state.rs            # connection state machine
│   └── types.rs            # CaError, CaValue
├── server/
│   ├── mod.rs              # re-exports
│   ├── ca_server.rs        # CaServer (top-level)
│   ├── ioc_app.rs          # adapter for IocApplication::run
│   ├── tcp.rs              # TCP listener + per-client task
│   ├── udp.rs              # UDP search responder
│   ├── beacon.rs           # RSRV_IS_UP emitter
│   └── monitor.rs          # subscription handling
├── repeater.rs             # CA repeater daemon
└── bin/                    # caget-rs, caput-rs, camonitor-rs, cainfo-rs, softioc-rs, ca-repeater-rs
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

## Testing

```bash
cargo test -p epics-ca-rs
```

Test coverage: header encode/decode (golden packets vs `caget`), DBR encoding for all type ranges, big-endian conversion, beacon timing, search request/response, virtual circuit handshake, monitor deadband filtering, extended header (>64 KB), origin tracking, multi-channel session.

## Dependencies

- epics-base-rs — PvDatabase, records, EpicsValue, DBR codec
- tokio — async runtime
- bytes — buffer management
- chrono — timestamp formatting
- thiserror — error types

## Requirements

- Rust 1.85+ (edition 2024)

## License

[EPICS Open License](../../LICENSE)
