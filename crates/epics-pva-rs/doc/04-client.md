# 04 — Client internals

The client is structured as a small collection of long-lived async
tasks plus a per-PV state machine. The public face is `PvaClient`;
internally it owns a `SearchEngine`, a `ConnectionPool`, and a map
of `Channel`s.

## Module map

| Module | Role |
|--------|------|
| `client_native/server_conn.rs` | Persistent TCP virtual circuit (reader / writer / heartbeat tasks). |
| `client_native/search_engine.rs` | UDP search broadcast, beacon listener, retry backoff, GUID blocklist. |
| `client_native/beacon_throttle.rs` | Beacon dedup + GUID-restart detection. |
| `client_native/channel.rs` | Per-PV state machine + `ConnectionPool`. |
| `client_native/ops_v2.rs` | GET / PUT / MONITOR / RPC implementations with auto-reconnect. |
| `decode.rs` (crate root, re-exported as `client_native::decode`) | Frame parser used by the server-conn reader task. |
| `client_native/context.rs` | Public `PvaClient` + `PvaClientBuilder`. |

## Task topology

```text
PvaClient (handle, cheap to clone)
   │
   ├── SearchEngine task (udp loop + retry + beacon listener)
   │       └── beacon socket task (when EPICS_PVA_BROADCAST_PORT bind succeeds)
   │
   └── ConnectionPool: Map<SocketAddr, Arc<ServerConn>>
           │
           └── ServerConn (one per upstream server)
                   ├── reader task   ── parses frames, dispatches to per-ioid waiters
                   ├── writer task   ── drains mpsc<Vec<u8>>
                   └── heartbeat     ── ECHO_REQUEST every 15s, idle timeout 30s
```

## Channel state machine

```text
   Idle
     │ ensure_active()
     ▼
   Searching ──▶ Connecting ──▶ Active { server, sid }
     ▲                                 │
     │ ServerConn closed               │
     └─────────────────────────────────┘
```

`Channel::ensure_active` is called by every op. Idempotent — multiple
concurrent calls converge on the same `Active` state via
`transition_lock`. Reconnect is automatic; monitor consumers see no
explicit interruption beyond the optional `MonitorEvent::Disconnected`
event.

Holdoff: after a full pass where every candidate failed to reach Active,
`reconnect_holdoff` (`channel.rs`) sets `holdoff_until` from the failure
class *and* the resolver kind, because pvxs paces the two kinds through
different machinery. A searched channel is paced by where it re-enters the
search ring — 10 buckets out for a pre-`CREATE_CHANNEL` TCP failure
(`SEARCH_RING_HOLDOFF`, ~10s at the 1Hz tick, `src/client.cpp:156-165`), the
current bucket for a `Creating`/`Active` disconnect (no holdoff), a full
revolution for a `CREATE_CHANNEL` refusal. A forced-server channel has no
ring: pvxs rebuilds its circuit and the `reconn` flag arms a flat 2s timer
before `startConnecting()` (`DIRECT_RECONNECT_HOLDOFF`,
`clientconn.cpp:26-32`). Subsequent `ensure_active` calls sleep out the
remainder before retrying; a successful Active clears it, so the next
disconnect re-derives its own pacing.

One deviation remains: a refused *direct* `CREATE_CHANNEL`. pvxs drops it
and retries only on the next circuit rebuild (`clientconn.cpp:386-392`),
which our pooled reconnect may never produce, so `DIRECT_REFUSAL_HOLDOFF`
substitutes a long fixed wait for that unbounded one. Neither the holdoffs
nor the deviation affects wire behaviour.

Multi-server failover: `find_all` collects every responder for a PV
within `MULTI_SERVER_WINDOW` (200ms). The fastest is tried first; the
rest stay as `alternatives` and are popped before falling back to a
fresh search.

## Search engine

| Constant | Default | Source |
|----------|---------|--------|
| `BACKOFF_SECS` | `[1, 1, 2, 5, 10, 15, 30, 60, 120, 210]` | R67: unused by engine; actual retry uses 30-bucket ring (~29 s cap) |
| `BEACON_TIMEOUT` | 360s | 2× pvxs beacon clean interval |
| `BEACON_CLEAN_INTERVAL` | 180s | matches pvxs `tickBeaconClean` |
| `MULTI_SERVER_WINDOW` | 200ms | per-search collection window |

The engine listens on:
- An ephemeral UDP socket for SEARCH_RESPONSE (per-process unique).
- A bound UDP socket on `EPICS_PVA_BROADCAST_PORT` (default 5076)
  for unsolicited beacons. SO_REUSEPORT lets multiple processes
  share the port. When the bind fails (port already taken without
  REUSEPORT), beacon-driven fast reconnect is silently disabled.

Beacon throttle: per-server-GUID seen-time map. A new GUID for an
already-known server address means "IOC restarted" — the engine
re-issues SEARCH for every channel currently disconnected on that
server. Mirrors pvxs `BeaconTracker`.

## ServerConn

One connection per `(server addr, optional TLS config)`. Inside:

- **Reader task**: parses frames from the socket; for each frame,
  consults a per-ioid router (`HashMap<u32, FrameTx>`) to forward
  to the waiting op. Transparently auto-acks `EchoRequest` control
  frames. Updates `last_rx_nanos` so the heartbeat can skip when
  data is flowing.
- **Writer task**: drains a bounded `mpsc::Sender<Vec<u8>>` into
  the socket. Bounded means a slow socket back-pressures every op,
  but in practice writes complete in microseconds; the queue depth
  is large enough to absorb bursts.
- **Heartbeat task**: sends `ECHO_REQUEST` every 15s; declares the
  connection dead if no bytes have arrived in 30s. On declaration,
  `cancel.cancel()` fires; reader/writer wind down; channels see
  `is_alive() == false` and transition to `Reconnecting`.

The router uses `mpsc::UnboundedSender` per ioid so a slow consumer
on a streaming op (monitor) doesn't block other ops on the same
connection. (This is a smell — see the kodex review memory; a slow
monitor pile-up could grow unbounded. Bounded with a "lagged" event
is the planned fix.)

Type cache: the connection **reader task** owns marker resolution. It
flattens `0xFD`/`0xFE` markers — in both the type descriptor region and
inside `any` DATA values — into self-contained inline frames, in strict
wire order, against one reader-owned cache plus a per-IOID introspection
map (`ServerConn::op_introspection`). `op_get` / `op_monitor` / `op_put`
then decode the routed frames with `decode_op_response` and an empty
cache — no shared op-side cache, no cross-op decode race. See
`crates/epics-pva-rs/doc/07-introspection-cache.md`. ChannelArray keeps an
op-local cache (its DATA layout is sub-op dependent).

## ops_v2

| Function | What it does |
|----------|--------------|
| `op_get` | INIT (sends pvRequest, gets type) → DATA → DESTROY. Single attempt. |
| `op_put` | INIT (gets type) → coerces value to match → DATA → reply → DESTROY. |
| `op_monitor` | INIT → START → loop receiving DATA frames → on disconnect, re-INIT/START on the next active connection. |
| `op_monitor_events` | Same as `op_monitor` but the callback receives typed `MonitorEvent::{Connected,Data,Disconnected,Finished}`. |
| `op_monitor_handle` | Wraps `op_monitor` in a pausable / queryable handle (`SubscriptionHandle`). |
| `op_rpc` | INIT (passes through pvRequest) → DATA (sends arg w/ its own type tag) → reply. |
| `op_get_field` | Fetches just the type descriptor; no value. |

GET / PUT / RPC are one-shot: a network failure surfaces as
`PvaError::Protocol` and the caller decides whether to retry.
MONITOR re-issues itself transparently on every reconnect; the only
caller-visible signal is the (optional) `MonitorEvent::Disconnected`
event.

## Public API surface

`PvaClient` exposes the pvxs methods at the same names. See
`09-pvxs-parity.md` for the full table. Highlights:

| pvxs | pva-rs |
|------|--------|
| `Context::get(name).exec()` | `pvget` / `pvget_full` / `pvget_fields` |
| `Context::put(name).set("k", v).exec()` | `pvput` |
| `Context::monitor(name).exec()` | `pvmonitor` / `pvmonitor_typed` / `pvmonitor_handle` / `pvmonitor_events` |
| `Context::rpc(name, args)` | `pvrpc` (when feature enabled) |
| `Context::hurryUp()` | `hurry_up` |
| `Context::cacheClear(name)` | `cache_clear` |
| `Context::ignoreServerGUIDs(...)` | `ignore_server_guids` |
| `DiscoverBuilder::pingAll(true)` | `ping_all` |
| `Context::close()` | `close` |
