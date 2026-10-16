# 03 — Client internals

This document walks through the client-side modules in `src/client/`.
For a higher-level overview see [`01-overview.md`](01-overview.md). For
state-machine details see [`05-state-machines.md`](05-state-machines.md).

## Module map

```
src/client/
├── mod.rs              CaClient, CaChannel, MonitorHandle, coordinator
├── search.rs           UDP search engine + nameserver TCP fan-out
├── transport.rs        Per-server TCP connection: read/write loop, echo
├── subscription.rs     SubscriptionRegistry, MonitorDeliveryOutcome
├── beacon_monitor.rs   Repeater-side beacon receiver, anomaly detection
├── state.rs            ChannelState enum, ChannelInner struct, events
└── types.rs            Internal Request/Event message enums
```

## Top-level: `CaClient` and `CaChannel`

`CaClient::new()` (`mod.rs:274`) bootstraps four long-lived tasks and
returns a handle to them. The returned struct holds:

```rust
pub struct CaClient {
    search_tx:        mpsc::UnboundedSender<SearchRequest>,
    transport_tx:     mpsc::UnboundedSender<TransportCommand>,
    coord_tx:         mpsc::UnboundedSender<CoordRequest>,
    diagnostics:      Arc<CaDiagnostics>,
    _coordinator:     JoinHandle<()>,
    _search_task:     JoinHandle<()>,
    _transport_task:  JoinHandle<()>,
    _beacon_task:     JoinHandle<()>,
}
```

The `_*` join handles are kept so that dropping the `CaClient` aborts
its tasks. Tasks communicate strictly through the named channels.

`CaClient::create_channel(name)` (`mod.rs:341`) is non-blocking: it
allocates a `cid`, registers it with the coordinator, schedules an
initial search, and returns a `CaChannel` immediately. Connection
state lives behind `CaChannel::wait_connected(timeout)`.

`CaChannel` exposes:

- `wait_connected(Duration)` — block until `state == Connected`
- `connection_events()` — `broadcast::Receiver<ConnectionEvent>` for
  reactive consumers
- `info() -> Option<ChannelInfo>` — current snapshot (no IO)
- `get()`, `get_with_timeout()` — READ_NOTIFY round trip
- `put()`, `put_with_timeout()`, `put_nowait()` — WRITE_NOTIFY or
  WRITE
- `subscribe()`, `subscribe_with_deadband(f64)` — EVENT_ADD

Drop semantics: `CaChannel::drop` sends `CoordRequest::DropChannel`,
which fans out `Unsubscribe` for each child subscription, then
`ClearChannel` to the server, then removes the channel.

## The coordinator

`run_coordinator` (`mod.rs:858`) is the heart of the client. It owns:

```rust
let mut channels:        HashMap<u32, ChannelInner> = HashMap::new();
let mut subscriptions:   SubscriptionRegistry      = ...;
let mut server_channels: HashMap<SocketAddr, HashSet<u32>> = ...;
let mut flow_control:    HashMap<SocketAddr, FlowControlState> = ...;
let mut read_waiters:    HashMap<u32, oneshot::Sender<...>> = ...;
let mut write_waiters:   HashMap<u32, oneshot::Sender<...>> = ...;
let mut pending_wait_connected: HashMap<u32, Vec<oneshot::Sender<()>>> = ...;
let mut pending_found:   HashMap<u32, SocketAddr> = ...;
```

A single `tokio::select!` multiplexes three input streams:

1. `coord_rx` — `CoordRequest` from `CaClient` / `CaChannel`
2. `search_rx` — `SearchResponse::Found` from search engine
3. `transport_rx` — `TransportEvent` from transport manager

### `CoordRequest` variants

| Variant | Producer | Effect |
|---------|----------|--------|
| `RegisterChannel` | `create_channel` | Insert `ChannelInner`, drain any early `pending_*` |
| `WaitConnected` | `wait_connected` | Resolve immediately if already Connected, else stash reply oneshot |
| `GetChannelInfo` | `get/put/subscribe` | Return current `ChannelSnapshot` |
| `Subscribe` | `subscribe_with_deadband` | Insert into `SubscriptionRegistry`; if Connected, send `TransportCommand::Subscribe` |
| `Unsubscribe` | `MonitorHandle::drop` | Send EVENT_CANCEL, remove from registry |
| `MonitorConsumed` | `MonitorHandle::recv` | Decrement per-server outstanding for flow control |
| `DropChannel` | `CaChannel::drop` | Cancel subs + ClearChannel + remove |
| `ReadNotify` | `get_with_timeout` | Stash `(ioid → reply_tx)` in `read_waiters` |
| `WriteNotify` | `put` | Same for `write_waiters` |
| `Shutdown` | `CaClient::drop`-time helper | Send ClearChannel for all live channels, exit loop |
| `ForceRescanServer` | beacon monitor | Re-search every `Disconnected`/`Searching` channel; echo-probe live circuits |

### Reverse server index

`server_channels: HashMap<SocketAddr, HashSet<u32>>` is updated on
every state transition involving an address (Found, ChannelCreated,
TcpClosed, Drop, ServerDisconn). It exists so that
`handle_disconnect` can quickly enumerate which channels belong to the
dead server without scanning every entry.

### Disconnect handling

`handle_disconnect` (`mod.rs:1338`) is the centerpiece of reconnection.
On `TransportEvent::TcpClosed { server_addr }`:

1. For every channel with `server_addr == this server`:
   - Mark `state = Disconnected`
   - Broadcast `ConnectionEvent::Disconnected`
   - Compute `reconnect_count` (incremented when previous connection
     was short-lived <30 s, reset otherwise)
   - Schedule a `SearchRequest::Schedule` with `initial_lane =
     min(reconnect_count, 8)` so the search engine applies a backoff +
     jitter
2. Mark all subscriptions for the affected cids as `needs_restore = true`
3. Fail every in-flight `read_waiter` / `write_waiter` for those cids
   with `CaError::Disconnected` so user code doesn't hang
4. Remove the address from `server_channels`

On reconnection (`TransportEvent::ChannelCreated`):

1. Update sid, native_type, element_count, server_addr,
   access_rights, last_connected_at
2. Wake all `connect_waiters`
3. Broadcast `AccessRightsChanged` + `Connected`
4. `subscriptions.restore_for_channel(cid, sid, ...)` re-issues
   EVENT_ADD for every subscription whose receiver is still alive,
   purges the rest as stale

## Search engine (`search.rs`)

### Algorithm

The search engine combines several layered policies:

1. **Per-channel exponential backoff lanes** — each channel sits at
   `lane_index ∈ [0, ∞)`. `lane_period(i) = base_rtte << i` clamped to
   `EPICS_CA_MAX_SEARCH_PERIOD` (default 300s, min 60s). Lane increases
   on every send.
2. **AIMD send budget** — `frames_per_try` starts high (50), AIMD
   evaluates each 1-second window (`AIMD_WINDOW`):
   - response rate >50% → +1 frame_per_try (additive increase)
   - response rate <10% → frame_per_try=1 (multiplicative decrease)
3. **Per-path RTT estimator** — Jacobson/Karels (RFC 6298) per
   destination. Used to size the lane base period. Floor at 32 ms
   (`MIN_RTT`).
4. **Penalty box** — after a failed TCP `CreateChannel`, the server
   address is in penalty for `PENALTY_DURATION` (30 s). Search
   responses from penalised servers are ignored so the channel can
   find another server.
5. **Datagram sequence validation** — outgoing VERSION header carries
   `data_type = 0x8000` (sequenceNoIsValid) plus `cid = seq_no`. We
   only accept SEARCH responses preceded by a VERSION whose seq_no
   matches (or by an unflagged VERSION, for legacy servers).

### Send loop

`send_due_searches` (`search.rs:627`) collects all channels whose
`next_deadline <= now`, builds one or more UDP datagrams under
`MAX_UDP_SEND` (1024 B), and sends to:

- Every entry in `addr_list` (UDP)
- Every TCP nameserver connection (one mpsc per connection,
  forwarded to long-lived TCP task)

`finalize_due_searches` advances each sent channel's `lane_index`,
reschedules to `now + lane_period(...)` (with optional fast-rescan
clamp during the 5-second window after a beacon anomaly).

### Nameserver connections (P4)

When `EPICS_CA_NAME_SERVERS` is set, `run_search_engine` spawns one
task per nameserver. Each task:

1. Opens TCP with 5 s connect timeout, exponential 1→30 s backoff on
   failure
2. Sends VERSION + HOST_NAME + CLIENT_NAME handshake
3. Receives outgoing search bytes via mpsc, writes them to TCP
4. Reads incoming bytes, forwards them via `tcp_response_tx` to the
   main search loop where the same `handle_udp_response` parser
   processes them
5. Sends an ECHO every 60 s of idle to keep the connection warm

Source: `search.rs::run_nameserver_connection`.

## Transport manager (`transport.rs`)

### `connect_server`

Establishes a new TCP connection (`transport.rs:283`):

1. `TcpStream::connect` with 5 s timeout
2. `set_nodelay(true)`
3. OS keepalive: 15 s idle, 5 s probe interval (`socket2`)
4. Build initial handshake frame: VERSION + HOST_NAME + CLIENT_NAME
5. Spawn a write task and a read task
6. Return `ServerConnection { write_tx, pending_frames, echo_probe, ... }`

### Read loop (`read_loop`)

Source: `transport.rs:401`. Implements:

- **30-second idle timeout** (`EPICS_CA_CONN_TMO`) → send ECHO
  request, switch to 5-second `echo_pending` mode
- **5-second echo timeout** (1st miss) → emit
  `TransportEvent::CircuitUnresponsive`, retry once
- **5-second echo timeout** (2nd miss) → emit `TcpClosed`
- **`echo_probe` notify** (from coordinator's beacon-anomaly path) →
  immediately enter echo_pending without waiting for idle timeout
- Reassemble framed messages, dispatch by `cmmd`
- 1 MB cap on accumulated buffer (DoS guard)

Per-opcode handling (just emits TransportEvent for the coordinator):

| In | Out (TransportEvent) |
|----|----------------------|
| VERSION | (cache `server_minor_version` for ECHO vs READ_SYNC choice) |
| ACCESS_RIGHTS | `AccessRightsChanged` |
| CREATE_CHAN | `ChannelCreated` |
| READ_NOTIFY | `ReadResponse` or `ReadError` (depending on cid==ECA_NORMAL) |
| WRITE_NOTIFY | `WriteResponse` |
| EVENT_ADD | `MonitorData` |
| ECHO / READ_SYNC | (just resets liveness, no event) |
| CREATE_CH_FAIL | `ChannelCreateFailed` |
| ERROR | `ServerError` |
| SERVER_DISCONN | `ServerDisconnect` |

### Write loop (`write_loop`)

Drains outgoing frames from mpsc, batches them into a single
`writer.write_all`, with a 10-second send timeout
(`SEND_BACKPRESSURE_FRAMES = 4096` triggers connection close on
backpressure). On any I/O error → emit `TcpClosed`.

### Backpressure

`pending_frames: AtomicUsize` is incremented on every queued frame and
decremented when the writer drains. If it climbs above 4096 the
connection is closed (rather than allowing the queue to grow without
bound). This is the same threshold C libca uses.

## Subscription registry (`subscription.rs`)

```rust
pub struct SubscriptionRegistry {
    subscriptions: HashMap<u32, SubscriptionRecord>,
}

pub struct SubscriptionRecord {
    pub subid: u32,
    pub cid: u32,
    pub data_type: Option<u16>,
    pub count: Option<u32>,
    pub mask: u16,
    pub server_addr: SocketAddr,
    pub callback_tx: mpsc::Sender<CaResult<Snapshot>>,   // bounded!
    pub needs_restore: bool,
    pub deadband: f64,
    pub last_value: Option<f64>,
    pub pending_deliveries: usize,
}
```

`callback_tx` capacity is `EPICS_CA_MONITOR_QUEUE` (default 256).
`on_monitor_data` does a `try_send`; on `Full` or `Closed` it returns
`MonitorDeliveryOutcome::Dropped` so the coordinator can bump
`CaDiagnostics::dropped_monitors`.

Client-side deadband (`deadband > 0`) suppresses scalar updates whose
absolute change is below the threshold. Disabled (0.0) by default.

`mark_disconnected(&[cid])` sets `needs_restore = true` and drains
each record's `pending_deliveries` (so flow-control accounting
resets). `restore_for_channel(cid, sid, ...)` re-issues EVENT_ADD for
every still-live subscription on that cid; entries with closed
`callback_tx` are purged as stale.

## Beacon monitor (`beacon_monitor.rs`)

`run_beacon_monitor` registers a UDP socket with the local repeater
(`localhost:5065`, retried 3× with 200 ms / 400 ms backoff). It then
reads 4 KB datagrams, walks every framed beacon, and updates a
`HashMap<SocketAddr, BeaconState>`.

Every 5 minutes of silence triggers a re-registration with the
repeater (in case the repeater was restarted).

`BeaconState` tracks `last_id`, `last_seen`, `period_estimate` (EWMA
α=0.25), and `count`. Anomaly when `beacon_id != last_id + 1` OR
`actual_interval < period_estimate / 3 && count > 3`.

Anomaly action: send `CoordRequest::ForceRescanServer` to the
coordinator. The coordinator:

- Re-searches every `Disconnected`/`Searching` channel (regardless of
  which server triggered the anomaly — beacons may use INADDR_ANY)
- Sends `TransportCommand::EchoProbe` to every connected server
  (deduplicated per address) so dead TCP circuits are detected within
  the 5 s echo window instead of the 30 s idle window.

## State module (`state.rs`)

```rust
pub enum ChannelState {
    Searching,    // UDP search ongoing
    Connecting,   // Found, TCP CREATE_CHAN in flight
    Connected,    // Ready for IO
    Unresponsive, // Echo timed out once; TCP still up
    Disconnected, // Connection lost; auto-research scheduled
    Shutdown,     // User dropped, no more reconnect
}
```

`ChannelInner` is the coordinator-private record:

```rust
pub(crate) struct ChannelInner {
    pub cid: u32,
    pub pv_name: String,
    pub state: ChannelState,
    pub sid: u32,
    pub native_type: Option<DbFieldType>,
    pub element_count: u32,
    pub server_addr: Option<SocketAddr>,
    pub access_rights: AccessRights,
    pub connect_waiters: Vec<oneshot::Sender<()>>,
    pub conn_tx: broadcast::Sender<ConnectionEvent>,
    pub reconnect_count: u32,
    pub last_connected_at: Option<Instant>,
}
```

`ConnectionEvent` is broadcast to every `connection_events()`
subscriber:

```rust
pub enum ConnectionEvent {
    Connected,
    Disconnected,
    Unresponsive,
    AccessRightsChanged { read: bool, write: bool },
}
```

## Diagnostics (`mod.rs::CaDiagnostics`)

```rust
pub struct CaDiagnostics {
    pub connections:           AtomicU64,
    pub disconnections:        AtomicU64,
    pub reconnections:         AtomicU64,
    pub unresponsive_events:   AtomicU64,
    pub subscriptions_restored:AtomicU64,
    pub subscriptions_stale:   AtomicU64,
    pub beacon_anomalies:      AtomicU64,
    pub search_requests:       AtomicU64,
    pub dropped_monitors:      AtomicU64,
    history: Mutex<Vec<DiagRecord>>, // ring buffer, capacity 256
}
```

`CaClient::diagnostics()` returns `DiagnosticsSnapshot` (a Display'able
struct). The history ring carries timestamped `DiagEvent` enums:
`Connected`, `Disconnected`, `Reconnected`, `Unresponsive`,
`Responsive`, `BeaconAnomaly`. Used by `ca-soak` and as the basis for
post-mortem analysis.

## Public API summary

```rust
let client = CaClient::new().await?;

// Channel ops
let ch = client.create_channel("PV:NAME");
ch.wait_connected(Duration::from_secs(5)).await?;
let info = ch.info();                          // current snapshot
let (typ, val) = ch.get_with_timeout(t).await?;
ch.put(&EpicsValue::Long(42)).await?;
ch.put_nowait(&value).await?;                  // CA_PROTO_WRITE (no ack)

// Subscriptions
let mut mon = ch.subscribe().await?;            // DBE_VALUE | DBE_LOG | DBE_ALARM
let mut mon = ch.subscribe_with_deadband(0.5).await?;
while let Some(ev) = mon.recv().await { ... }

// Reactive connection state
let mut events = ch.connection_events();
while let Ok(ev) = events.recv().await { ... }

// Diagnostics
let snap = client.diagnostics();
println!("{}", snap);

// Convenience one-shots
client.caget("PV:NAME").await?;                 // create+connect+get+drop
client.caput("PV:NAME", value, timeout).await?; // create+connect+put+drop
```

## Common pitfalls

- **Don't call `put` without `wait_connected` first.** `put` checks
  `state.is_operational()` and returns `Disconnected` if the channel
  hasn't reached `Connected` yet.
- **`MonitorHandle` is RAII.** Dropping it sends EVENT_CANCEL; if you
  want a long-lived monitor, hold the handle.
- **`CaClient::drop` aborts tasks.** In-flight reads/writes that
  haven't sent yet may be lost. Use the `Shutdown` request via
  `client.shutdown().await` for graceful termination if needed.
- **`EPICS_CA_AUTO_ADDR_LIST=NO` without an explicit address list**
  results in the search engine sending no UDP traffic. Set
  `EPICS_CA_ADDR_LIST` explicitly in that case.
