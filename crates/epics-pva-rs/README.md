# epics-pva-rs

Pure Rust implementation of the [pvAccess](https://docs.epics-controls.org/projects/pvaccess/en/latest/) protocol — modern EPICS structured data transport.

No C dependencies. Just `cargo build`.

**Repository:** <https://github.com/epics-rs/epics-rs>

## Overview

pvAccess is the next-generation EPICS protocol that supersedes Channel Access for structured data. Where CA carries primitive scalars and 1D arrays, PVA carries arbitrary nested structures (NormativeTypes like NTScalar, NTEnum, NTTable, NTNDArray, ...) — making it the natural choice for areaDetector images, MASAR snapshots, structured machine state, and any data richer than a single scalar.

```
PVA Client (pvget-rs, OPI, Python)
       │
       │  UDP search (port 5076)
       │  TCP virtual circuit (port 5075)
       │  pvData wire format (FieldDesc + values, BitSet deltas)
       │
       ▼
PVA Server (epics-pva-rs server)
       │
       ▼
ChannelSource (epics-bridge-rs BridgeProvider)
       │
       ▼
PvDatabase (epics-base-rs records)
```

## Architecture

The crate is layered: a byte-level protocol foundation (`proto`), a pvData
runtime/codec model (`pvdata`), NormativeTypes builders (`nt`), native client
and server runtimes (`client_native` / `server_native`), and the CLI
binaries.

```
epics-pva-rs/src/
├── lib.rs
├── error.rs            # PvaError, PvaResult
├── proto/              # byte-level wire primitives (no epics-base-rs dep)
│   ├── buffer.rs       #   endian-aware read/write over bytes::{Buf,BufMut}
│   ├── size.rs         #   variable-length size encoding (Size)
│   ├── string.rs       #   length-prefixed UTF-8 strings
│   ├── status.rs       #   operation status codes
│   ├── header.rs       #   8-byte PVA frame header (PvaHeader)
│   ├── command.rs      #   command codes + QoS subcommand flags
│   ├── bitset.rs       #   BitSet for monitor delta encoding
│   ├── selector.rs     #   field selectors used by pvRequest
│   └── ip.rs           #   IPv4/IPv6 <-> 16-byte PVA address conversion
├── pvdata/             # pvData runtime model + wire encoding
│   ├── scalar.rs       #   ScalarType, ScalarValue
│   ├── structure.rs    #   PvField, PvStructure, UnionItem, VariantValue
│   ├── field.rs        #   FieldDesc, Member, TypeDef introspection
│   ├── typed_array.rs  #   TypedScalarArray
│   ├── value.rs        #   typed Value <-> ScalarValue conversion
│   └── encode.rs       #   FieldDesc + value wire encode/decode
├── nt/                 # NormativeTypes builders (NTScalar, NTEnum, ...)
├── codec.rs            # application-level message builders over proto
├── pv_request.rs       # pvRequest builders (field-selection structs)
├── client.rs           # thin re-export of client_native (PvaClient)
├── client_native/      # native pvAccess client runtime
│   ├── decode.rs       #   frame parsing
│   ├── server_conn.rs  #   persistent TCP virtual circuit
│   ├── search.rs       #   UDP search broadcast / reply
│   ├── search_engine.rs#   beacon-driven discovery
│   ├── beacon_throttle.rs # beacon rate limiting
│   ├── channel.rs      #   per-PV state machine + connection pool
│   ├── operation.rs    #   operation primitives
│   ├── ops_v2.rs       #   GET / PUT / MONITOR / RPC / GET_FIELD drivers
│   └── context.rs      #   public PvaClient facade
├── server/             # IOC-facing server entry points
│   ├── pva_server.rs   #   PvaServer / PvaServerBuilder wrapper
│   └── native_source.rs#   PvDatabaseSource (PvDatabase -> ChannelSource)
├── server_native/      # native pvAccess server runtime
│   ├── runtime.rs      #   PvaServer, PvaServerConfig, run_pva_server
│   ├── tcp.rs / udp.rs #   TCP virtual circuits / UDP search responder
│   ├── source.rs       #   ChannelSource trait
│   ├── composite.rs    #   CompositeSource (multi-source fan-in)
│   ├── shared_pv.rs    #   SharedPV / SharedSource
│   └── peers.rs        #   PeerRegistry connection tracking
├── auth/               # AuthZ / transport security
│   ├── plain.rs        #   username/host "ca" AuthZ
│   └── tls.rs          #   rustls-backed TLS (pvas://, opt-in)
├── config/             # EPICS_PVA_* / EPICS_PVAS_* env config
│   ├── env.rs
│   └── mod.rs
├── service/            # axum-style PVA RPC service framework
├── cli.rs / format.rs / log.rs   # CLI helpers, output formatting, logging
└── bin/                # 8 command-line binaries (see below)
```

## Modules

### Wire protocol (`proto/`)
Byte-level foundation with zero dependency on `epics-base-rs` or higher-level
types, so protocol code can be exercised with raw fixtures. Layered after
pvxs `src/pvaproto.h`: endian-aware buffers, variable-length size encoding,
length-prefixed strings, status codes, the 8-byte PVA frame header, command
codes + QoS subcommand flags, the monitor-delta `BitSet`, field selectors,
and IPv4/IPv6 address conversion.

### pvData (`pvdata/`)
The pvData runtime model and its wire codec:

- **ScalarType** — Boolean, Byte/UByte, Short/UShort, Int/UInt, Long/ULong,
  Float, Double, String
- **ScalarValue** — runtime value of any scalar type
- **PvField** — recursive runtime field (scalar, scalar array, structure,
  union)
- **PvStructure** — composite with `struct_id` (e.g. `"epics:nt/NTScalar:1.0"`)
  and ordered named fields
- **FieldDesc / Member / TypeDef** — type description (no values) for
  `getField` introspection
- **encode** — `FieldDesc` and value wire encoding/decoding

### NormativeTypes (`nt/`)
Wire-compatible builders for the standard PVA structure IDs: NTScalar /
NTScalarArray, NTEnum, NTTable, NTURI (RPC argument passing), NTAttribute
(used by NTNDArray), and NTNDArray (areaDetector images). Each module
produces both the `FieldDesc` introspection and a `PvField` value.

### Codec (`codec.rs`)
Application-level message builders — a thin layer over `proto` that produces
the byte sequences expected by clients (`build_search`, `build_get_init`, ...)
and servers (`build_connection_validated`).

### Client (`client_native/`, re-exported as `client`)
Native pvAccess client runtime with no external client dependency. Frame
decode, a persistent TCP virtual circuit, UDP search + beacon-driven
discovery, a per-PV state machine with connection pooling, GET / PUT /
MONITOR / RPC / GET_FIELD operation drivers (with automatic monitor
reconnect), and the public `PvaClient` facade. `crate::client` is a thin
re-export so callers can use `crate::client::PvaClient`.

### Server (`server/`, `server_native/`)
`server_native` is the native pvAccess server runtime: `PvaServer` /
`PvaServerConfig`, TCP virtual circuits, the UDP search responder, the
`ChannelSource` trait, `CompositeSource` for multi-source fan-in, `SharedPV`,
and a `PeerRegistry`. `server/` provides the IOC-facing wrapper
(`PvaServer` / `PvaServerBuilder`) and `PvDatabaseSource`, which adapts an
`epics-base-rs` `PvDatabase` into a `ChannelSource`.

### Auth (`auth/`)
`plain` implements username/host "ca" AuthZ negotiated by every connection
today. `tls` adds opt-in rustls-backed TLS for `pvas://`, reading cert/key
paths from the standard `EPICS_PVA{,S}_TLS_*` environment variables.

### Config (`config/`)
Mirrors pvxs's `Config::fromEnv()` for the standard `EPICS_PVA_*` (client) and
`EPICS_PVAS_*` (server) environment variables.

### Service (`service/`)
An axum-style PVA RPC service framework. The `PvaService` trait plus the
`#[pva_service]` attribute macro (from `epics-macros-rs`) hide request
decoding and response encoding so service authors write plain typed
`async fn`s.

## CLI Tools

The crate builds 8 binaries (the 4 explicitly declared in `Cargo.toml` plus
4 auto-discovered from `src/bin/`). They mirror the pvxs `tools/` set:

- **pvget-rs** — read PVA channel values (single shot)
- **pvput-rs** — write a PVA channel value
- **pvmonitor-rs** — subscribe to PVA channel updates
- **pvinfo-rs** — display PVA channel structure metadata
- **pvcall-rs** — RPC client; builds an NTURI request from `field=value`
  pairs (mirrors pvxs `pvcall`)
- **pvlist-rs** — server discovery via passive beacon listen or active ping
  (mirrors pvxs `pvlist`)
- **pvxvct-rs** — PV Access Virtual Cable Tester; decodes SEARCH/BEACON UDP
  frames for network diagnostics (mirrors pvxs `pvxvct`)
- **mshim-rs** — beacon multicast shim; forwards UDP datagrams between
  endpoints to bridge IPv4 multicast (mirrors pvxs `mshim`)

## Quick Start

```bash
# Read a PVA channel
pvget-rs MY:PV

# Subscribe
pvmonitor-rs MY:PV

# Get field type info
pvinfo-rs MY:PV

# Put
pvput-rs MY:PV 42.5

# Call an RPC method
pvcall-rs MY:RPC gain=2.5

# Discover servers
pvlist-rs -w 5
```

### Library

```rust
use epics_pva_rs::client::PvaClient;

let client = PvaClient::new()?;
let pv = client.get("MY:PV").await?;
if let Some(val) = pv.get_value() {
    println!("{val}");
}
```

## Environment Variables

`config/mod.rs` mirrors pvxs `Config::fromEnv()`. The common variables:

| Variable | Default | Purpose |
|----------|---------|---------|
| `EPICS_PVA_ADDR_LIST` | (empty) | Comma/whitespace-separated unicast SEARCH targets |
| `EPICS_PVA_AUTO_ADDR_LIST` | `YES` | Append per-NIC broadcast addresses |
| `EPICS_PVA_INTF_ADDR_LIST` | (all) | Interfaces to bind to |
| `EPICS_PVA_SERVER_PORT` | `5075` | Server TCP port |
| `EPICS_PVA_BROADCAST_PORT` | `5076` | UDP search/beacon port |
| `EPICS_PVA_NAME_SERVERS` | (empty) | TCP-based name servers (`host:port` list) |
| `EPICS_PVA_CONN_TMO` | `30` | Connection idle timeout (seconds) |

Server-side `EPICS_PVAS_*` variables (e.g. `EPICS_PVAS_INTF_ADDR_LIST`,
`EPICS_PVAS_BEACON_ADDR_LIST`) fall back to their `EPICS_PVA_*` counterparts.
TLS paths use `EPICS_PVA{,S}_TLS_*`.

## Server

A PVA server is built from a `PvDatabase` via `PvaServerBuilder` and run with
`PvaServer::run`, or with a custom `ChannelSource` via `run_with_source`:

```rust
use epics_pva_rs::server::PvaServer;

let server = PvaServer::builder()
    .port(5075)
    .pv("MY:PV", EpicsValue::from(0.0))
    .build()        // async, returns CaResult<PvaServer>
    .await?;
server.run().await?;
```

To serve an existing `epics-base-rs` `PvDatabase`, drive `run_with_source`
with a `PvDatabaseSource`; an IOC running CA and PVA together selects over
both server futures.

## Testing

```bash
cargo test -p epics-pva-rs
```

## Dependencies

- tokio — async runtime
- bytes — refcounted byte buffers for zero-copy monitor fan-out
- rustls / tokio-rustls — opt-in TLS transport
- chrono — timestamps
- clap — CLI argument parsing
- thiserror — error types

## Requirements

- Rust 1.85+ (edition 2024)

## License

[EPICS Open License](../../LICENSE)
</content>
</invoke>
