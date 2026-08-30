# 01 — Architecture overview

`epics-ca-rs` implements **EPICS Channel Access (CA)** version 4.13 over
TCP/UDP for both client and server roles. It is a pure Rust workspace
crate with no FFI to libca.

## Top-level layout

```
crates/epics-ca-rs/
├── src/
│   ├── lib.rs              re-exports + public surface
│   ├── channel.rs          public AccessRights, ChannelInfo, alloc_*
│   ├── protocol.rs         CA wire constants + CaHeader (parse/serialize)
│   ├── repeater.rs         CA repeater daemon + ensure_repeater client helper
│   ├── client/
│   │   ├── mod.rs          CaClient, CaChannel, MonitorHandle, coordinator loop
│   │   ├── search.rs       UDP search engine (AIMD, RTT, penalty, nameserver TCP)
│   │   ├── transport.rs    per-server TCP I/O, echo heartbeat, keepalive
│   │   ├── subscription.rs SubscriptionRegistry, monitor coalescing
│   │   ├── beacon_monitor.rs
│   │   ├── state.rs        ChannelState, ChannelInner, ConnectionEvent
│   │   └── types.rs        internal request/event message enums
│   ├── server/
│   │   ├── mod.rs          run_ca_ioc entrypoint
│   │   ├── ca_server.rs    CaServer + CaServerBuilder
│   │   ├── tcp.rs          TCP listener, per-client dispatch loop
│   │   ├── udp.rs          UDP search responder (one task per interface)
│   │   ├── beacon.rs       Beacon emitter (multi-NIC fan-out)
│   │   ├── monitor.rs      FlowControlGate + spawn_monitor_sender
│   │   ├── addr_list.rs    EPICS_CAS_* parsing, broadcast discovery
│   │   └── ioc_app.rs      Re-export shim for ioc_app::IocApplication
│   └── bin/
│       ├── caget-rs.rs     Rust caget
│       ├── caput-rs.rs     Rust caput
│       ├── camonitor-rs.rs Rust camonitor
│       ├── cainfo-rs.rs    Rust cainfo
│       ├── ca-repeater-rs.rs CA repeater daemon
│       ├── softioc-rs.rs   Rust softIoc
│       └── ca-soak.rs      Long-running soak test driver
├── tests/                  unit, integration, interop, stress
└── benches/                criterion benchmarks
```

## Conceptual model

CA is a **two-tier protocol**:

1. **UDP search** — clients broadcast `CA_PROTO_SEARCH` to find which
   server hosts a given PV name. Servers reply unicast.
2. **TCP virtual circuit** — once located, the client opens a TCP
   connection to the server and uses it for all subsequent operations
   (read/write/subscribe).

Beacons (`CA_PROTO_RSRV_IS_UP`) are broadcast UDP datagrams the server
emits on a slow ramp; clients use them to detect IOC restarts and
re-search affected channels.

## Client architecture

The client is composed of four long-lived `tokio` tasks plus per-channel
state:

```
                         ┌───────────────────────────────┐
                         │       CaClient (struct)       │
                         │   coord_tx, search_tx, etc.   │
                         └─────────────┬─────────────────┘
                                       │
            ┌──────────────────────────┼──────────────────────────┐
            ▼                          ▼                          ▼
  ┌──────────────────┐      ┌────────────────────┐    ┌──────────────────┐
  │ search engine    │      │ coordinator        │    │ transport mgr    │
  │ (UDP + nameserv) │◀────▶│ (ChannelInner map) │◀──▶│ (per-server TCP) │
  └──────────────────┘      └────────────────────┘    └──────────────────┘
                                       ▲
                                       │
                            ┌──────────────────────┐
                            │ beacon monitor (UDP) │
                            │ on local repeater    │
                            └──────────────────────┘
```

Single-writer ownership: only the coordinator mutates channel state.
Other tasks send `CoordRequest` enums (`client/types.rs`) over an mpsc
channel.

### Tasks

| Task | Source | Responsibility |
|------|--------|----------------|
| coordinator | `client/mod.rs::run_coordinator` | Owns the `HashMap<cid, ChannelInner>`. Routes `CoordRequest` → search/transport. Handles all `TransportEvent`. Drives reconnect on disconnect. |
| search engine | `client/search.rs::run_search_engine` | UDP socket, AIMD send budget, per-server RTT estimator, penalty box, optional TCP nameserver fan-out. |
| transport mgr | `client/transport.rs::run_transport_manager` | Maintains a TCP connection per server address. Each connection has its own read/write task pair, echo heartbeat, keepalive, send watchdog. |
| beacon monitor | `client/beacon_monitor.rs::run_beacon_monitor` | Registers with the local CA repeater, parses incoming beacons, raises anomaly events to the coordinator. |

### Public surface

```rust
let client = CaClient::new().await?;
let ch = client.create_channel("MY:PV");
ch.wait_connected(Duration::from_secs(5)).await?;
let (_, val) = ch.get_with_timeout(Duration::from_secs(3)).await?;
ch.put(&EpicsValue::Long(42)).await?;
let mut monitor = ch.subscribe().await?;
while let Some(snap) = monitor.recv().await { ... }
```

`CaChannel` and `MonitorHandle` are RAII; dropping them sends
`ClearChannel` / `Unsubscribe` to the coordinator.

## Server architecture

The server is built around the same Tokio task model:

```
                ┌─────────────────────────────┐
                │       CaServer (struct)     │
                │     PvDatabase, port, acf   │
                └──────────────┬──────────────┘
                               │
       ┌──────────────────┬────┴───┬──────────────────┬──────────────────┐
       ▼                  ▼        ▼                  ▼                  ▼
  ┌─────────┐      ┌───────────┐ ┌─────────────┐ ┌────────────┐ ┌────────────┐
  │ TCP     │      │ UDP per-  │ │ Beacon      │ │ Scan       │ │ Autosave   │
  │ listener│      │ interface │ │ emitter     │ │ scheduler  │ │ manager    │
  │         │      │ responder │ │ (multi-NIC) │ │            │ │            │
  └────┬────┘      └───────────┘ └─────────────┘ └────────────┘ └────────────┘
       │
       │ accept()
       ▼
   ┌────────────────────────┐
   │ per-client task        │
   │ handle_client()        │
   │ ClientState (channels, │
   │ subs, flow_control)    │
   └────────────────────────┘
```

### Tasks

| Task | Source | Responsibility |
|------|--------|----------------|
| TCP listener | `server/tcp.rs::run_tcp_listener` | Accepts new clients, sets keepalive (15s/5s) on accepted sockets, spawns one `handle_client` task per connection, broadcasts `ServerConnectionEvent::Connected/Disconnected`. |
| Per-client | `server/tcp.rs::handle_client` | Owns one client's `ClientState`. Reads framed CA messages, dispatches to per-opcode handlers, enforces inactivity timeout + max-channel/max-subs caps. |
| UDP responder | `server/udp.rs::run_udp_search_responder` | Spawns one bound socket per interface in `EPICS_CAS_INTF_ADDR_LIST`. Replies to `CA_PROTO_SEARCH`, sends `CA_PROTO_NOT_FOUND` when `CA_DO_REPLY` is requested and PV is missing. |
| Beacon emitter | `server/beacon.rs::run_beacon_emitter` | Sends `CA_PROTO_RSRV_IS_UP` to every destination in `EPICS_CAS_BEACON_ADDR_LIST` ∪ auto-discovered NIC broadcasts. Exponential 20ms→`EPICS_CAS_BEACON_PERIOD` ramp. Resets on accept/disconnect. |
| Monitor sender | `server/monitor.rs::spawn_monitor_sender` | Per-subscription forwarder: drains coalesce slot first, then mpsc, encodes DBR, writes EVENT_ADD response. |

### Per-client state (`ClientState`)

```rust
struct ClientState {
    channels:       HashMap<sid, ChannelEntry>,     // PV bindings
    subscriptions:  HashMap<sub_id, SubscriptionEntry>,
    channel_access: HashMap<sid, AccessLevel>,
    next_sid:       AtomicU32,
    hostname:       HostIdentity,  // Claimed(client name) unless asCheckClientIP / mTLS
    username:       String,
    acf:            Arc<Option<AccessSecurityConfig>>,
    tcp_port:       u16,
    client_minor_version: u16,
    flow_control:   Arc<FlowControlGate>,
}
```

## Cross-cutting concerns

### Single-writer ownership

Both client coordinator and per-client server state run on a single
task. All mutation is local; cross-task communication is via mpsc/oneshot
channels carrying `enum` requests. This avoids locking entirely on the
hot path.

### Bounded queues

- Client subscription mpsc: `EPICS_CA_MONITOR_QUEUE` (default 256)
- Server PV subscriber mpsc: 64 (with coalesce slot for drop-oldest)
- All other internal channels: `mpsc::unbounded` (low traffic, bounded
  by the number of channels).

### TCP framing

All framing on TCP uses `CaHeader::from_bytes_extended` which handles
both the standard 16-byte header and the extended 24-byte header used
when `postsize == 0xFFFF` (large arrays). See `02-wire-protocol.md`.

### Error model

`CaError` (`crates/epics-base-rs/src/error.rs`) is the unified result
type.
Wire-level errors carry an ECA code (61 codes total, see
`protocol.rs::ECA_*`). `CaError::WriteFailed(eca)` is the common
"server replied with an ECA error" case.

### Diagnostics

`CaDiagnostics` (`client/mod.rs`) is an `Arc`-shared atomic counter
struct shared between coordinator and search engine. The user obtains
a snapshot via `CaClient::diagnostics()`. Counters cover connections,
disconnections, reconnections, unresponsive events, beacon anomalies,
search requests, dropped monitors, plus a 256-entry timestamped event
ring buffer.

## Where libca-parity is enforced

The port deliberately mirrors libca behaviour rather than tidying it up.
The cases that matter most:

- **Search response parsing**: accept `cid == 0` and `cid == ~0u32` as
  "use UDP source address" (`client/search.rs::handle_udp_response`).
- **CA_PROTO_NOT_FOUND** when client sets `CA_DO_REPLY=10`
  (`server/udp.rs`, `server/tcp.rs`).
- **Beacon fan-out** uses `discover_broadcast_addrs()` to mirror
  `osiSockDiscoverBroadcastAddresses`.
- **DBR_PUT_ACKT/ACKS** route to record `ACKT`/`ACKS` fields, never to
  the channel's normal write path.
- **Client host name**: under C's default (`asCheckClientIP == 0`) the
  server stores the name the client sent in `CA_PROTO_HOST_NAME` and does
  not verify it; only `asCheckClientIP = 1` pins the peer's dotted-quad IP
  instead. There is no `EPICS_CAS_USE_HOST_NAMES` variable, here or in
  epics-base — see `08-environment.md`.

## Performance notes

The hot path is a single `tokio::select!` per task. There are no
locks on the message-dispatch path (only the per-server `BufWriter`
mutex on the server side, and the `last_value` slot on subscriptions).

Per-channel allocations:

- Client: 1 `ChannelInner` (~256 B) + 1 broadcast::Sender + 1 search
  state record. ~1 KB total.
- Server: 1 `ChannelEntry` + per-subscription `SubscriptionEntry` +
  one `tokio::task` per subscription.

A typical IOC with 10⁴ channels uses ≪ 50 MB.
