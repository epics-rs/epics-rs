# 04 — Server internals

This document covers the server side under `src/server/`. The server
is responsible for hosting PVs (process variables) and answering CA
clients. It is built around a `tokio` task model parallel to the
client (see [`03-client.md`](03-client.md)).

## Module map

```
src/server/
├── mod.rs       run_ca_ioc entrypoint, public re-exports
├── ca_server.rs CaServer + CaServerBuilder, run() top-level loop
├── tcp.rs       TCP listener + per-client handle_client dispatch
├── udp.rs       UDP search responder (one task per interface)
├── beacon.rs    Beacon emitter (multi-NIC fan-out)
├── monitor.rs   FlowControlGate + spawn_monitor_sender
├── addr_list.rs EPICS_CAS_* parsing + broadcast discovery
└── ioc_app.rs   Re-export shim for IocApplication consumers
```

## Top level: `CaServer`

`CaServer` (`ca_server.rs:150`) bundles the runtime state for an IOC:

```rust
pub struct CaServer {
    db:                Arc<PvDatabase>,
    port:              u16,
    acf:               Arc<Option<AccessSecurityConfig>>,
    autosave_config:   Option<autosave::SaveSetConfig>,
    autosave_manager:  Option<Arc<autosave::AutosaveManager>>,
    conn_events:       Option<broadcast::Sender<ServerConnectionEvent>>,
    after_init_hooks:  Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}
```

### Builder pattern

`CaServerBuilder` (`ca_server.rs:24`) wraps an internal
`epics_base_rs::server::ioc_builder::IocBuilder` so most IOC-level
methods (`pv`, `record`, `db_file`, `register_device_support`, etc.)
delegate to the IOC builder. CA-specific knobs:

- `port(u16)` — server UDP/TCP port (default 5064)
- `acf(...)`, `acf_file(path)` — access security configuration
- `register_subroutine(name, fn)` — for `sub` records

`CaServerBuilder::build()` returns a `CaServer` ready to run.

### Running

`CaServer::run()` (`ca_server.rs:289`) is the top-level lifecycle
loop:

1. Build a `ScanScheduler` (epics-base-rs) for record processing
2. Optionally start an `AutosaveManager`
3. Parse `addr_list::from_env()` to get UDP/beacon configuration
4. Spawn:
   - TCP listener task (returns the actual bound port via oneshot)
   - UDP responder task (one per interface)
   - Beacon emitter task
5. Run a `tokio::select!` over (UDP, TCP, beacon, scan scheduler).
   First exit propagates as the `run()` return value; the others are
   aborted.

`run_with_shell` is the same plus an iocsh REPL on a separate thread
for interactive control.

## TCP listener (`tcp.rs::run_tcp_listener`)

Binds `0.0.0.0:port` (falls back to ephemeral port if in use). On each
`accept()`:

1. Notify the beacon emitter to reset its interval (so client churn is
   visible to CA clients quickly via beacon anomaly)
2. Broadcast `ServerConnectionEvent::Connected(peer)` to subscribers
   (used by ca-gateway)
3. Enable OS-level TCP keepalive (15 s idle / 5 s probe)
4. Spawn a per-client task running `handle_client(stream, peer, ...)`

When `handle_client` returns, broadcast
`ServerConnectionEvent::Disconnected(peer)` and re-notify the beacon
emitter (for fast-restart visibility).

## Per-client dispatch (`tcp.rs::handle_client`)

This is where one TCP virtual circuit lives.

```rust
async fn handle_client(
    stream: TcpStream,
    peer: SocketAddr,
    db: Arc<PvDatabase>,
    acf: Arc<Option<AccessSecurityConfig>>,
    tcp_port: u16,
) -> CaResult<()>
```

### State

```rust
struct ClientState {
    channels:        HashMap<sid, ChannelEntry>,
    subscriptions:   HashMap<sub_id, SubscriptionEntry>,
    channel_access:  HashMap<sid, AccessLevel>,
    next_sid:        AtomicU32,
    hostname:        HostIdentity,  // Claimed(name) by default; Pinned(ip|cert) otherwise
    username:        String,
    acf:             Arc<Option<AccessSecurityConfig>>,
    tcp_port:        u16,
    client_minor_version: u16,
    flow_control:    Arc<FlowControlGate>,
}
```

`hostname` is the circuit's access-security identity, and its *source* is
decided once, at the start of the handler — exactly where C decides it, in
`create_tcp_client` (`caservertask.c:1425-1439`). The two variants encode
whether the client may still set it, so the illegal transition is not
representable rather than guarded at the write site:

* `Claimed(name)` — C's default (`asCheckClientIP == 0`): empty until the
  client sends `CA_PROTO_HOST_NAME`, then whatever it claimed, stored
  unconditionally (`camessage.c:797-878`). `HAG` entries are host names in
  this mode, so this is what a `HOST(node)` rule matches.
* `Pinned(value)` — the peer's dotted-quad IP under `asCheckClientIP = 1`
  (`CA_PROTO_HOST_NAME` is then ignored, `camessage.c:839-843`), or an
  mTLS-verified certificate identity, which outranks both C modes.

Set the mode with the `asCheckClientIP` iocsh command *before* `asInit` —
the HAG storage form is chosen when the ACF is parsed. See
`08-environment.md`. An earlier revision of this document claimed the port
primed `hostname` from the peer IP "to match C rsrv default"; that was
backwards, and the `EPICS_CAS_USE_HOST_NAMES` variable it named exists
nowhere in epics-base (R7-16).

### Read loop

```rust
loop {
    let n = match tokio::time::timeout(inactivity_timeout(), reader.read(&mut buf)).await {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => break, // EPICS_CAS_INACTIVITY_TMO expired
    };
    if n == 0 { break }                         // EOF
    accumulated.extend_from_slice(&buf[..n]);
    if accumulated.len() > max_accumulated() { break } // DoS guard

    // Frame and dispatch as many full messages as we can.
    while let Some(msg) = parse_frame(&accumulated, &mut offset) {
        dispatch_message(...).await?;
    }
    accumulated.drain(..offset);
}
```

`max_accumulated()` mirrors the client-side cap and prevents a hostile
or buggy client from declaring a huge `postsize` and streaming nothing
else (which would otherwise grow the Vec without bound). It is sized at
`max_payload_size()` (default 16 MB, honours `EPICS_CA_MAX_ARRAY_BYTES`)
plus the extended header and a 64 KiB slack, so it never rejects a
legal large waveform — the cap is strictly above the largest frame the
protocol can carry.

`inactivity_timeout()` reads `EPICS_CAS_INACTIVITY_TMO` (default
600 s, minimum 30 s).

### Cleanup on exit

When the read loop ends:

1. Abort every spawned monitor task
2. For each subscription: call `pv.remove_subscriber(sub_id)` so the
   PV's subscriber list doesn't keep stale `Sender`s

### Per-opcode handlers

`dispatch_message` (`tcp.rs:264`) is a big `match` on `hdr.cmmd`.
Highlights:

| Opcode | Behaviour |
|--------|-----------|
| `VERSION` | Cache `client_minor_version`. Reply with our version. |
| `HOST_NAME` | `state.hostname.claim(name)` — taken unless the identity is `Pinned` (C `host_name_action`). Re-evaluate access rights. |
| `CLIENT_NAME` | Update `state.username`. Re-evaluate access rights. |
| `CREATE_CHAN` | DoS guard: refuse if `len(channels) >= EPICS_CAS_MAX_CHANNELS`. Look up PV in `db`, allocate sid, build `ChannelEntry`, send `ACCESS_RIGHTS` + `CREATE_CHAN` response (or `CREATE_CH_FAIL` on miss). |
| `READ` / `READ_NOTIFY` | Same data path; only the response header differs (deprecated `READ` puts sid in `cid`, while `READ_NOTIFY` puts ECA status in `cid`). |
| `WRITE` / `WRITE_NOTIFY` | Decode payload, route to `pv.set` (simple PV) or `db.put_record_field_from_ca` (record). For DBR_PUT_ACKT/PUT_ACKS, route to record's ACKT/ACKS field. Reply with WRITE_NOTIFY when `is_notify`. |
| `EVENT_ADD` | DoS guard on per-channel sub count. Spawn `monitor_sender` task. Send initial snapshot. |
| `EVENT_CANCEL` | Abort task, remove from registry, send final EVENT_ADD with count=0. |
| `EVENTS_OFF` / `EVENTS_ON` | Pause/resume `FlowControlGate`. |
| `READ_SYNC` | Flush buffered output (legacy echo). |
| `ECHO` | Echo back. |
| `SEARCH` | (TCP path) v4.4+ only. PV lookup. Reply with TCP search response or NOT_FOUND if `CA_DO_REPLY`. |
| `CLEAR_CHANNEL` | Cancel all child subs, drop the channel, reply CLEAR_CHANNEL. |
| _other_ | Reply CA_PROTO_ERROR with ECA_INTERNAL. |

### WRITE_NOTIFY async path

When a record's processing is asynchronous (e.g. motor records that
schedule a move and complete later), the write handler spawns a
detached task to await completion before sending the WRITE_NOTIFY
reply. This prevents a slow record from blocking the per-client read
loop and freezing all monitors on that connection.

```rust
if let Some(rx) = completion_rx {
    let writer_c = writer.clone();
    tokio::spawn(async move {
        let _ = rx.await;
        let mut resp = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
        ...
        let _ = writer_c.lock().await.write_all(&resp.to_bytes()).await;
    });
}
```

Source: `tcp.rs:572`.

## UDP search responder (`udp.rs`)

`run_udp_search_responder` spawns one task per interface in
`EPICS_CAS_INTF_ADDR_LIST` (default: `0.0.0.0`). Each task binds a
UDP socket to that interface + the configured port, sets
`SO_REUSEADDR` (and `SO_REUSEPORT` on macOS), and answers SEARCH
requests:

```rust
loop {
    let (len, src) = socket.recv_from(&mut buf).await?;
    if ignore_addrs.contains(src.ip()) { continue; }
    for each framed CA_PROTO_SEARCH in buf {
        let pv_name = parse_payload(...);
        if db.has_name(&pv_name).await {
            send_search_reply(...);                  // VERSION + SEARCH + 8B
        } else if hdr.data_type == CA_DO_REPLY {
            send_not_found(...);                     // NOT_FOUND header
        }
    }
}
```

### `local_ip_for(remote)`

To populate the `cid` field of a SEARCH reply with our routable IP,
we open a temporary unconnected UDP socket and call `connect(remote)`.
The OS picks an outgoing interface based on routing; we read back
`local_addr()` and use that. If anything fails we fall back to
`0.0.0.0`, in which case clients use the UDP packet's source address
(which is what `cid==0` signals).

This is the equivalent of libca's
`epicsSocketEnumerateInterfaces` heuristic for the reply IP.

## Beacon emitter (`beacon.rs`)

`run_beacon_emitter`:

```rust
async fn run_beacon_emitter(
    server_port: u16,
    beacon_addrs: Vec<SocketAddr>,
    max_period: Duration,
    reset: Arc<Notify>,
) -> CaResult<()>
```

Algorithm:

1. Bind a fresh UDP socket, set `SO_BROADCAST`
2. Determine our routable IP (same `connect`-and-read-local trick as
   above)
3. Loop:
   - Send `RSRV_IS_UP` to every entry in `beacon_addrs`
   - `tokio::select!` between `sleep(interval)` and `reset.notified()`
     - sleep elapsed → `interval = min(interval * 2, max_period)`
     - reset → `interval = 20 ms`
   - Increment `beacon_id`

The 20 ms initial period mirrors libca's "fast restart" — bursts of
beacons make beacon-anomaly detection on receivers reliable. After
~10 doublings the period asymptotes to `EPICS_CAS_BEACON_PERIOD`
(default 15 s).

`reset` is notified on every accept/disconnect by the TCP listener
(`tcp.rs:153, 160`) so client churn is immediately visible to CA
clients.

`beacon_addrs` is the union of `EPICS_CAS_BEACON_ADDR_LIST` (or
`EPICS_CA_ADDR_LIST` fallback) and `discover_broadcast_addrs()`
when `EPICS_CAS_AUTO_BEACON_ADDR_LIST != NO`. See the next section.

## Address-list parser (`addr_list.rs`)

```rust
pub struct CasUdpConfig {
    pub intf_addrs:    Vec<Ipv4Addr>,
    pub beacon_addrs:  Vec<SocketAddr>,
    pub ignore_addrs:  Vec<Ipv4Addr>,
    pub beacon_period: Duration,
}

pub fn from_env() -> CasUdpConfig
```

Honoured environment variables:

| Variable | Effect |
|----------|--------|
| `EPICS_CAS_INTF_ADDR_LIST` | UDP responder bind interfaces (default `0.0.0.0`) |
| `EPICS_CAS_BEACON_ADDR_LIST` | Explicit beacon destinations |
| `EPICS_CA_ADDR_LIST` | Fallback when `BEACON_ADDR_LIST` is unset |
| `EPICS_CAS_AUTO_BEACON_ADDR_LIST` | `YES` (default) → also include per-NIC broadcasts |
| `EPICS_CAS_IGNORE_ADDR_LIST` | UDP source IPs to drop |
| `EPICS_CAS_BEACON_PERIOD` | Steady-state beacon period (sec, default 15) |
| `EPICS_CA_REPEATER_PORT` | Beacon destination port (default 5065) |

`discover_broadcast_addrs()` uses the `if-addrs` crate and returns
every IPv4 broadcast on every up, non-loopback interface. When the
crate fails (unsupported OS) the result is `vec![]` and we fall back
to `255.255.255.255` only.

## Monitor sender (`monitor.rs`)

Each EVENT_ADD on the server spawns a per-subscription task via
`spawn_monitor_sender(pv, sub_id, data_type, writer, flow_control, rx)`.

Loop body:

```rust
loop {
    let next = if let Some(ev) = pv.pop_coalesced(sub_id).await {
        Some(ev)               // drain coalesce slot first
    } else {
        rx.recv().await        // then mpsc
    };
    let Some(mut event) = next else { break };
    if flow_control.is_paused() {
        // keep coalescing while EVENTS_OFF active
        event = flow_control.coalesce_while_paused(&mut rx, event).await?;
    }
    encode_dbr(...).write_all(&padded).await?;
}
```

### `FlowControlGate`

```rust
pub struct FlowControlGate {
    paused:  AtomicBool,
    resumed: Notify,
}
```

`pause()` is called when the client sends EVENTS_OFF; `resume()` on
EVENTS_ON. While paused, `coalesce_while_paused` drains every newly
arriving event (`try_recv`) keeping only the most recent, so when the
client unpauses it sees the *latest* value rather than a backlog.

This is the **server-side companion** to the client's coalesce slot
(see `07-flow-control.md`).

## Subscription overflow coalescing

`epics-base-rs::server::pv::Subscriber` carries an
`Arc<Mutex<Option<MonitorEvent>>>` slot used for "drop-oldest, keep-newest"
semantics under producer overload:

- When the bounded mpsc(64) is full, `try_send` fails. The producer
  stores the latest event in the coalesce slot, overwriting any
  previous overflow value.
- The consumer (`spawn_monitor_sender`) drains the slot before each
  `rx.recv()` so a slow client always converges on the current PV
  value.

Both `ProcessVariable` and `RecordInstance` have matching
`pop_coalesced(sid)` accessors.

## Per-client DoS guards

| Guard | Default | Variable |
|-------|---------|----------|
| Inactivity timeout | 600 s | `EPICS_CAS_INACTIVITY_TMO` |
| Accumulated buffer cap | `max_payload_size()` + 64 KiB | `EPICS_CA_MAX_ARRAY_BYTES` |
| Max channels per client | unbounded (opt-in) | `EPICS_CAS_MAX_CHANNELS` |
| Max subscriptions per channel | unbounded (opt-in) | `EPICS_CAS_MAX_SUBS_PER_CHAN` |

The channel and subscription caps are **unbounded by default** to match
C rsrv, which imposes no per-client channel count limit
(`claim_ciu_action`) and no per-channel subscription count limit
(`event_add_action`) — both refuse only on genuine memory exhaustion.
Set the corresponding variable to opt into a fixed cap. Exceeding a
*configured* cap (or hitting the buffer/timeout guards) results in
either a CREATE_CH_FAIL response, an ECA_ALLOCMEM error, or a clean TCP
close — never an unbounded memory allocation.

## Connection event broadcast

`CaServer::connection_events()` returns a
`broadcast::Receiver<ServerConnectionEvent>` so external components
(e.g. ca-gateway) can react to every accepted client. The TCP listener
publishes `Connected(peer)` on accept and `Disconnected(peer)` after
`handle_client` returns, regardless of how it ended.

The broadcast channel has capacity 64; slow consumers will see
`RecvError::Lagged(n)` rather than backpressure the listener.

## Environment summary

The server-relevant env vars are documented exhaustively in
[`08-environment.md`](08-environment.md). The most consequential ones
for operational behaviour:

- `EPICS_CAS_INTF_ADDR_LIST`, `EPICS_CAS_BEACON_ADDR_LIST` — multi-NIC
  topology
- `EPICS_CAS_BEACON_PERIOD` — beacon cadence
- `asCheckClientIP` (iocsh command, not an env var) — ACF host-identity source
- `EPICS_CAS_INACTIVITY_TMO` — half-open client cleanup horizon
- `EPICS_CAS_MAX_CHANNELS`, `EPICS_CAS_MAX_SUBS_PER_CHAN` — DoS caps
