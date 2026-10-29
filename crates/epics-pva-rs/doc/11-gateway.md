# 11 — PVA gateway

`epics-bridge-rs::pva_gateway` is the Rust port of the legacy C++
PVA gateway `pva2pva/p2pApp`. It runs an upstream `PvaClient` and a
downstream `PvaServer` in the same process, deduplicating monitor
subscriptions across many downstream clients.

This page is the architectural reference. End-user usage is in
`crates/epics-bridge-rs/src/pva_gateway/mod.rs` (rustdoc) and the
`pva-gateway-rs` binary `--help`.

## Topology

```text
   downstream PVA clients
            │
            ▼
   ┌──────────────────┐         ┌────────────────────────┐
   │ PvaServer (DS)   │  uses   │ GatewayChannelSource   │
   │ in pva-rs        │────────▶│ (impl ChannelSource)   │
   └──────────────────┘         └──────────┬─────────────┘
                                           │ lookup / get / put
                                           ▼
                                ┌────────────────────────┐
                                │ ChannelCache           │
                                │  PV → UpstreamEntry    │
                                │   ├ broadcast::Sender  │  fan-out
                                │   └ monitor task       │  (one per PV)
                                └──────────┬─────────────┘
                                           │ pvmonitor / pvput
                                           ▼
                                ┌────────────────────────┐
                                │ PvaClient (US)         │
                                │ in pva-rs              │
                                └──────────┬─────────────┘
                                           ▼
                                  upstream PVA servers
```

## Components

| File | Type / function |
|------|-----------------|
| `channel_cache.rs` | `ChannelCache` — `Mutex<HashMap<PV, Arc<UpstreamEntry>>>`, periodic 30s cleanup. |
| `channel_cache.rs` | `UpstreamEntry` — one upstream monitor task + tokio broadcast sender + cached snapshot + first-event Notify. |
| `source.rs` | `GatewayChannelSource` — implements `ChannelSource`; bridges downstream ops to the cache. |
| `gateway.rs` | `PvaGateway` / `PvaGatewayConfig` — builds + owns the cache + downstream server. |
| `error.rs` | `GwError` — typed gateway-side errors. |
| `bin/pva_gateway_rs.rs` | `pva-gateway-rs` daemon binary. |

## Lifecycle of a downstream op

### Search (`has_pv`)

1. Downstream PVA client UDP-broadcasts `SEARCH name`.
2. The gateway's downstream `PvaServer` UDP responder calls
   `GatewayChannelSource::has_pv(name)`.
3. `has_pv` calls `ChannelCache::lookup(name, connect_timeout)`.
4. Cache miss → spawn an upstream monitor task; insert entry; wait on
   `first_event` notify with `connect_timeout` (default 5s).
5. Cache hit (already populated) → instant return.
6. Returning `Ok` → server emits SEARCH_RESPONSE; downstream client
   opens TCP and starts ops.

The lookup race is solved by holding the cache mutex across spawn +
insert so two concurrent searches for the same PV never spawn two
upstream monitor tasks.

### Get (`get_value`)

1. `GatewayChannelSource::get_value` calls `ChannelCache::lookup`
   (fast path now — entry already populated).
2. Returns the cached `entry.snapshot()` — the same value the
   upstream server would return on a fresh GET, no extra round-trip.

### Monitor (`subscribe`)

1. Source `subscribe(name)` looks up the entry, calls
   `entry.subscribe()` to get a fresh `broadcast::Receiver`.
2. Spawns a per-subscriber bridge task that:
   - sends the cached snapshot first (so the downstream client
     immediately sees the current value, matching pvxs),
   - then forwards every broadcast event into a fresh `mpsc::Sender`,
   - on `RecvError::Lagged(n)` (slow consumer) it swallows and
     continues; the next upstream tick resyncs the cache,
   - on `RecvError::Closed` (entry dropped) it exits.
3. The downstream server's monitor pipeline drains the mpsc and
   serializes each event into a `MonitorElement` frame.

One upstream subscription serves N downstream subscribers. Mirrors
p2pApp `MonitorCacheEntry` / `MonitorUser` fan-out.

### Put (`put_value`)

1. Source `put_value(name, value)` ensures the entry exists
   (`ChannelCache::lookup`).
2. Converts `PvField` to the string form `pvput` accepts via
   `pvfield_to_pvput_string` (scalar / scalar-array / NTScalar
   `.value`).
3. Calls `client.pvput(name, value_str)` — reuses the existing
   upstream channel, no fresh CREATE_CHAN.

## Cache cleanup

A 30s tick (matches p2pApp `cacheClean`) scans every entry:

- `drop_poke == true` → reset to false and keep ("recently used").
- `drop_poke == false && subscriber_count == 0` → evict.

`drop_poke` is bumped by every `lookup` / `subscribe`. So an entry
that's been completely idle for one full tick AND has no live
broadcast subscribers is dropped; the upstream monitor task is
aborted via `AbortOnDrop`.

## Quick start

```rust
use std::sync::Arc;
use std::time::Duration;
use epics_base_rs::runtime::task::Period;
use epics_bridge_rs::pva_gateway::{PvaGateway, PvaGatewayConfig};
use epics_pva_rs::server_native::PvaServerConfig;

# async fn run() -> epics_bridge_rs::pva_gateway::error::GwResult<()> {
let cfg = PvaGatewayConfig {
    upstream_client: None,                                  // builds default
    server_config: PvaServerConfig {
        tcp_port: 5076,                                     // downstream listen
        udp_port: 5077,
        ..PvaServerConfig::default()
    },
    cleanup_interval: Period::new(Duration::from_secs(30)).unwrap(),
    connect_timeout: Duration::from_secs(5),
};
let gw = PvaGateway::start(cfg)?;
gw.run().await?;
# Ok(())
# }
```

Or via the binary:

```bash
pva-gateway-rs --tcp-port 5076 --udp-port 5077 -vv
```

## Gaps vs pvxs / p2pApp

The current implementation covers the read / monitor / put paths.
Open work:

- **No pvlist (allow/deny)** — every PV name searched downstream gets
  proxied. p2pApp has `pvlist` filtering. Add a regex- or glob-based
  matcher in `ChannelCache::lookup` before spawning the upstream
  monitor.
- **No GUID-based server tracking** — relies on `PvaClient`'s own
  beacon-driven reconnect. pvxs gateway tracks upstream GUIDs to
  detect IOC restarts; we get the same effect via the underlying
  client.
- **No ACL on PUT** — `is_writable` returns `true` unconditionally.
  Wire up via `PvaServerConfig::auth_complete` or a gateway-level
  config.
- **PUT is string-form only** — `pvfield_to_pvput_string` converts
  `PvField` → str → `pvput`. Lossy for compound types; awaiting a
  typed `pvput_field` API in `pva-rs::client::PvaClient`.
- **No server-side stats publication** — pvxs gateway publishes
  `gw:status:*` introspection PVs. Not yet implemented; can wire up
  via a `SharedSource` registered alongside `GatewayChannelSource`
  in a `CompositeSource` setup.
- **Per-subscriber moncache queue with overflow accumulation** — p2pApp
  accumulates change/overrun bits into an "overflow element" when the
  downstream queue is full. We rely on tokio broadcast's lagged
  semantics, which drops events but doesn't merge them into a single
  catch-up frame. Acceptable for steady-state; misses the "tell me
  what changed while I was away" guarantee.
