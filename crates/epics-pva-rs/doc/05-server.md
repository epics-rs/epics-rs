# 05 — Server internals

The server is split into a low-level `server_native` (raw protocol +
`ChannelSource` trait) and a high-level `server` module that wires a
`PvDatabase` to the protocol layer for record-aware IOCs.

## Module map

| Module | Role |
|--------|------|
| `server_native/runtime.rs` | `PvaServer` / `PvaServerConfig` — bind UDP + TCP, spawn responder + listener tasks. |
| `server_native/tcp.rs` | Per-connection state machine + op dispatcher. |
| `server_native/udp.rs` | Search responder + periodic beacon emitter. |
| `server_native/shared_pv.rs` | `SharedPV` (mailbox) + `SharedSource` (named registry). |
| `server_native/source.rs` | `ChannelSource` trait + `DynSource` = `Arc<dyn ChannelSourceObj>`. |
| `server_native/composite.rs` | Multi-source registry with priority order. |
| `server/pva_server.rs` | High-level builder for record-aware IOCs. |
| `server/native_source.rs` | `PvDatabaseSource` — adapter from `PvDatabase` to `ChannelSource`. |

## Task topology

```text
PvaServer
   ├── UDP responder task   (run_udp_responder_with_config)
   │     └── beacon emitter ── periodic SEARCH_RESPONSE-like Beacon to all destinations
   │
   └── TCP listener task     (accept loop, spawn handle_client per accept)
         │
         └── per-client { reader, writer, heartbeat } subtasks
```

The TCP per-client write side is a single dedicated task draining a
bounded `mpsc<Vec<u8>>`; every emit site (read loop, monitor sub
tasks, heartbeat) pushes framed bytes into the channel and the task
serializes them onto the socket. Replaces the older
`Arc<Mutex<Writer>>` so a slow client cannot hold the lock against
unrelated emit sites.

## ChannelSource trait

```rust
pub trait ChannelSource: Send + Sync + 'static {
    async fn list_pvs(&self) -> Vec<String>;
    async fn has_pv(&self, name: &str) -> bool;
    async fn get_introspection(&self, name: &str) -> Option<FieldDesc>;
    async fn get_value(&self, name: &str) -> Option<PvField>;
    async fn put_value(&self, name: &str, value: PvField) -> Result<(), String>;
    async fn is_writable(&self, name: &str) -> bool;
    async fn subscribe(&self, name: &str) -> Option<mpsc::Receiver<PvField>>;
    async fn rpc(&self, name: &str, req_desc: FieldDesc, req: PvField)
        -> Result<(FieldDesc, PvField), String> { Err(...) }
}
```

This is the only contract `server_native::tcp::handle_op` cares
about. Implement it for any data backing — `SharedSource` (mailbox),
`PvDatabaseSource` (record DB), `GatewayChannelSource`
(epics-bridge-rs/src/pva_gateway), `CompositeSource` (multi-backend
priority routing).

## Per-connection state

Each accepted TCP connection runs one `handle_connection_io` task
that owns a `HashMap<sid, ChannelState>`. Each `ChannelState`
contains:

```rust
struct ChannelState {
    name: String,
    cid: u32,
    sid: u32,
    introspection: Option<FieldDesc>,    // learned at CREATE_CHANNEL
    ops: HashMap<ioid, OpState>,         // GET / PUT / MONITOR per ioid
}

struct OpState {
    intro: FieldDesc,
    kind: OpKind,                        // Get / Put / Monitor / Rpc
    monitor_started: bool,
    monitor_abort: Option<Arc<AbortOnDrop>>,
    mask: BitSet,                        // from pvRequest at INIT
}
```

When the connection ends, the `HashMap` drops; each `OpState` drops;
each `monitor_abort` drops; the spawned monitor subscriber tasks are
aborted automatically.

## Op dispatch

`server_native/tcp.rs::handle_op` receives the raw payload and
switches on subcmd bits (see [`02-wire-protocol.md`](02-wire-protocol.md)):

- INIT (`subcmd & 0x08`): decode pvRequest (type + value), translate
  to a `BitSet` mask via `pv_request::request_to_mask`, store the
  `OpState`, emit `ioid + subcmd + Status + (introspection unless RPC)`.
- DATA (no INIT bit):
  - `Get` — call `source.get_value(name)`, encode with the saved
    mask via `encode_pv_field_with_bitset`.
  - `Put` — decode the bitset + value, call `source.put_value`.
    If subcmd has `0x40` (PUT-GET), echo the post-put value back.
  - `Monitor` — on first start (subcmd `0x40` / `0x80` / `0x00`),
    spawn a subscriber task; transitions are no-ops afterwards.
    The task drains `source.subscribe(name)`, squashes overflow,
    reports watermark crossings, and emits `MonitorElement` frames.
  - `Rpc` — decode arg type + value; call `source.rpc(name, desc, val)`;
    emit response.
- DESTROY_REQUEST: removes the `OpState` (drops the abort guard).
- CANCEL_REQUEST: stops the running monitor but keeps the OpState so
  a fresh START can re-spawn.

## SharedPV

A mailbox PV implementation that anyone (record handler, RPC
implementation, test harness) can `open(desc, value)` and `try_post`
into. Subscribers are tracked as `mpsc::Sender<PvField>`; a
`force_post` retains receivers with full queues (drop-on-full),
`try_post` keeps the slow ones too (next post may overwrite). User
hooks: `on_first_connect`, `on_last_disconnect`, `on_put`, `on_rpc`.
Mirrors pvxs `sharedpv.cpp::SharedPV`.

## SharedSource / CompositeSource

`SharedSource`: name → `SharedPV` map. The standard backing for
ad-hoc PV servers and tests.

`CompositeSource`: `(order, name) -> Arc<dyn ChannelSourceObj>` map,
consulted in ascending key order (pvxs `serverconn.h:267`,
`src/server.cpp:91`), so equal orders tie-break by source name.
`has_pv` returns true if any source has it; `get_value` returns the
first match. Used by the bridge crate's dual-protocol IOC mode and by
sites that mix QSRV-served records with hand-built SharedPVs.

## Monitor pipeline

Per-monitor-subscriber spawn structure:

```text
   client subscribes
        │
        ▼
   handle_op MONITOR-INIT (stores OpState, no task yet)
        │
        ▼
   handle_op MONITOR-START spawns subscriber task:
        │     ├── drains source.subscribe(name) (mpsc<PvField>)
        │     ├── squashes if more than queue_depth events stack up
        │     ├── tracks high/low watermarks
        │     └── encodes + pushes via tx (mpsc<Vec<u8>> to writer task)
        │
        ▼
   on connection end / DESTROY_REQUEST / CANCEL_REQUEST:
        │
        └── monitor_abort drops → task aborted → no orphan spawn
```

`PvaServerConfig::monitor_queue_depth` (default 4) is the squash
threshold; `monitor_high_watermark` (default 64) is the warning
trigger surfaced via `tracing::warn`.

## Auth + connection validation

On accept, the server sends `SetByteOrder` + `CONNECTION_VALIDATION`
request advertising `["ca", "anonymous"]`. The client replies with a
chosen method + payload. `parse_client_credentials` (`tcp.rs:213`)
extracts `user`, `host`, and `roles` (POSIX groups for the `ca`
method). The resolved `ClientCredentials` is passed to the
optional `auth_complete` hook configured on `PvaServerConfig`. ACL
gating belongs in this hook; the protocol-level layer always
`Status::ok()`s.

## Beacons + change_count

Periodic UDP fan-out of a 4-byte payload (flags / seq / change_count /
addr / port / proto). `change_count` is bumped when the set of names
returned by `source.list_pvs()` differs from the previous tick; clients
re-issue searches on change_count change even when sequence is in
lock-step. Mirrors pvxs `server.cpp::doBeacons`.

The auto-beacon path resolves destinations once at startup via
`config::env::list_broadcast_addresses(udp_port)`; restart the server
to pick up new NICs (pvxs re-resolves on interface change — TODO).
