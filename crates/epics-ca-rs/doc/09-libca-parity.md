# 09 — libca / rsrv parity matrix

How `epics-ca-rs` compares to the C reference implementation in
`epics-base/modules/ca/` (libca) and `epics-base/modules/database/src/ioc/rsrv/`
(rsrv).

This is the canonical answer to "is it a drop-in replacement for libca?"
The short version: **functionally yes for operational scenarios, no for
the C ABI surface**. Details below.

## Wire-protocol parity

| Feature | libca / rsrv | epics-ca-rs | Notes |
|---------|--------------|-------------|-------|
| All standard opcodes (VERSION, EVENT_ADD/CANCEL, READ/READ_NOTIFY, WRITE/WRITE_NOTIFY, CREATE_CHAN, CREATE_CH_FAIL, ACCESS_RIGHTS, CLEAR_CHANNEL, ECHO, READ_SYNC, EVENTS_OFF/ON, ERROR, RSRV_IS_UP, SEARCH, NOT_FOUND, SERVER_DISCONN, REPEATER_REGISTER/CONFIRM) | ✅ | ✅ | Bit-for-bit compatible |
| Extended header (postsize=0xFFFF) | ✅ | ✅ | Used for >65535-element arrays |
| DBR families: Plain, STS, TIME, GR, CTRL | ✅ | ✅ | All 7 native types in each family |
| Alarm-acknowledge: PUT_ACKT, PUT_ACKS, STSACK_STRING | ✅ | ✅ | Server routes ACKT/ACKS to record fields |
| Deprecated CA_PROTO_READ (cmd=3) | ✅ | ✅ | Server still answers; client doesn't emit |
| Deprecated CA_PROTO_READ_BUILD (cmd=16) | ✅ | ❌ | Modern clients don't emit; we drop on receive |
| Deprecated CA_PROTO_SIGNAL (cmd=25) | ✅ | ❌ | Same |
| `cid == 0` and `cid == ~0u32` "address unknown" sentinels in SEARCH replies | ✅ | ✅ | Both treated as "use UDP source address" |
| 8-byte payload alignment | ✅ | ✅ | Required by C client TCP parser |
| ECA codes | 61 codes | 61 codes | Verbatim from caerr.h |
| `ca_message(status)` text strings | ✅ | ✅ (`eca_message`) | Verbatim from access.cpp |

## Connection lifecycle

| Feature | libca / rsrv | epics-ca-rs |
|---------|--------------|-------------|
| Search backoff (exponential per channel) | ✅ | ✅ |
| `EPICS_CA_MAX_SEARCH_PERIOD` cap | ✅ | ✅ |
| RTT-aware base period | ✅ Jacobson/Karels | ✅ Jacobson/Karels (RFC 6298) |
| AIMD search budget | ✅ | ✅ |
| Penalty box on TCP connect failure | ✅ | ✅ (30 s) |
| Datagram sequence validation | ✅ | ✅ |
| Beacon anomaly detection (out-of-sequence beacon ID, fast period) | ✅ | ✅ |
| Beacon-driven re-search | ✅ | ✅ |
| Echo heartbeat | ✅ | ✅ (30 s idle, 5 s timeout) |
| READ_SYNC fallback for pre-v4.3 servers | ✅ | ✅ |
| Reconnection storm jitter | ✅ | ✅ (0–50% of lane period) |
| Subscription auto-restore on reconnect | ✅ | ✅ |
| Stale subscription cleanup (closed callback) | ✅ | ✅ |
| `disconnectGovernorTimer` style backoff | ✅ | ✅ (lane 1..8 on bulk disconnect) |

## Discovery and addressing

| Feature | libca / rsrv | epics-ca-rs |
|---------|--------------|-------------|
| `EPICS_CA_ADDR_LIST` parsing | ✅ | ✅ |
| `EPICS_CA_AUTO_ADDR_LIST` per-NIC discovery | ✅ `osiSockDiscoverBroadcastAddresses` | ✅ via `if-addrs` |
| `EPICS_CA_NAME_SERVERS` (TCP nameserver) | ✅ | ✅ |
| `EPICS_CAS_INTF_ADDR_LIST` (multi-NIC server bind) | ✅ | ✅ |
| `EPICS_CAS_BEACON_ADDR_LIST` (beacon destinations) | ✅ | ✅ |
| `EPICS_CAS_AUTO_BEACON_ADDR_LIST` | ✅ | ✅ |
| `EPICS_CAS_IGNORE_ADDR_LIST` | ✅ | ✅ |
| `EPICS_CAS_BEACON_PERIOD` | ✅ | ✅ |
| Repeater (5065) registration + fan-out | ✅ | ✅ |
| In-process repeater fallback | ✅ | ✅ |

## Flow control and queueing

| Feature | libca / rsrv | epics-ca-rs |
|---------|--------------|-------------|
| EVENTS_OFF / EVENTS_ON | ✅ | ✅ |
| Flow control keyed on OS socket-buffer occupancy | ✅ `bytesArePendingInOS` | ✅ `read_loop` FIONREAD probe |
| Contiguous-busy-frame trigger | ✅ `maxContiguousFrames` | ✅ `protocol::max_contiguous_frames()` |
| Trigger scaled from `EPICS_CA_MAX_ARRAY_BYTES` | ✅ `cac.cpp:233-237` | ✅ |
| Immediate `EVENTS_ON` the first time the socket reads clean | ✅ | ✅ |
| Per-consumer outstanding-monitor counter / hysteresis | ❌ (libca has neither) | ❌ |
| Server-side per-subscription event queue | ✅ ringbuffer | ✅ mpsc(64) + coalesce slot |
| Server-side drop-oldest, keep-newest | ✅ | ✅ |
| Client-side bounded subscription queue | ✅ (ca_event_queue) | ✅ mpsc(`EPICS_CA_MONITOR_QUEUE`) |
| Send-side TCP backpressure | ✅ `flushBlockThreshold` | ✅ 4096-frame cap |
| Send timeout | ✅ `tcpSendWatchdog` | ✅ 10 s |

## Access security

| Feature | libca / rsrv | epics-ca-rs |
|---------|--------------|-------------|
| ACF parser (UAG/HAG/ASG/RULE) | ✅ | ✅ |
| Hostname-based access | ✅ | ✅ |
| Username-based access | ✅ | ✅ |
| INPA-INPL evaluation in rules | ✅ | ⚠️ partial |
| `asCheckClientIP` toggle | ✅ (iocsh var) | ✅ (iocsh var; default `0`, client-claimed name authoritative) |
| Reverse-DNS lookup | ⚠️ (most builds skip) | ❌ |
| Late access-rights re-evaluation on HOST/CLIENT_NAME change | ✅ | ✅ |
| `CA_PROTO_ACCESS_RIGHTS` event broadcast | ✅ | ✅ |

## Resource caps and DoS guards

| Feature | libca / rsrv | epics-ca-rs |
|---------|--------------|-------------|
| Max accumulated TCP buffer per client | implicit | ✅ 1 MB |
| Per-client inactivity timeout | partial (OS keepalive) | ✅ `EPICS_CAS_INACTIVITY_TMO` |
| Max channels per client | implicit (memory) | implicit by default; opt-in `EPICS_CAS_MAX_CHANNELS` |
| Max subscriptions per channel | implicit | implicit by default; opt-in `EPICS_CAS_MAX_SUBS_PER_CHAN` |
| TCP keepalive on accepted sockets | ⚠️ off by default | ✅ enabled (15/5/3) |
| Max payload size cap | ✅ `EPICS_CA_MAX_ARRAY_BYTES` | ✅ `EPICS_CA_MAX_ARRAY_BYTES` |

## Diagnostics and observability

| Feature | libca / rsrv | epics-ca-rs |
|---------|--------------|-------------|
| Connection counters | partial (CA tools) | ✅ `CaDiagnostics` |
| Disconnection / reconnection counters | partial | ✅ |
| Unresponsive event counter | partial | ✅ |
| Beacon anomaly counter | partial | ✅ |
| Dropped monitor counter | ❌ | ✅ |
| Recent-event ring buffer | ❌ | ✅ (256 entries) |
| Connection lifecycle broadcast (server) | ❌ | ✅ `ServerConnectionEvent` |
| Per-channel `CaChannel::info()` snapshot | partial via `ca_*` accessors | ✅ |

## Multi-context / priority

| Feature | libca / rsrv | epics-ca-rs |
|---------|--------------|-------------|
| `ca_create_channel(name, ..., priority, ...)` priority param | ✅ | ❌ (single virtual circuit per server) |
| Per-priority TCP virtual circuits | ✅ | ❌ |
| Per-priority OS thread priority | ✅ (limited effect on Linux without RT) | ❌ (tokio runtime is pool-shared anyway) |
| `ca_create_context` / `ca_attach_context` | ✅ | ❌ (each `CaClient` is its own context) |
| Multiple priorities to same IOC over separate circuits | ✅ | ❌ |

The `priority` parameter is rarely used outside of large facilities
that need QoS isolation between control loops and bulk monitors.
[`05-state-machines.md`](05-state-machines.md) and the `epics-base`
docs explain its semantics; if your site needs it, the workaround is
to instantiate two `CaClient` objects with different `EPICS_CA_*`
configurations.

## C ABI surface (`ca_*` API)

`epics-ca-rs` exposes a Rust-native API only. There is no C-callable
wrapper.

| Function family | libca | epics-ca-rs equivalent |
|-----------------|-------|------------------------|
| `ca_context_create` | ✅ | `CaClient::new()` |
| `ca_create_channel` | ✅ | `CaClient::create_channel` |
| `ca_array_get` / `ca_array_get_callback` | ✅ | `CaChannel::get_with_timeout` |
| `ca_array_put` / `ca_array_put_callback` | ✅ | `CaChannel::put` / `put_with_timeout` / `put_nowait` |
| `ca_create_subscription` | ✅ | `CaChannel::subscribe` / `subscribe_with_deadband` |
| `ca_clear_subscription` | ✅ | drop `MonitorHandle` |
| `ca_clear_channel` | ✅ | drop `CaChannel` |
| `ca_pend_io` / `ca_pend_event` / `ca_poll` | ✅ | `await` (the runtime drives the loop) |
| `ca_message(status)` | ✅ | `protocol::eca_message(status)` |
| `ca_state` / `ca_field_type` / `ca_element_count` / `ca_name` / `ca_host_name` / `ca_read_access` / `ca_write_access` | ✅ | `CaChannel::info()` returns a `ChannelInfo` struct |
| `ca_replace_printf_handler` / `ca_replace_access_rights_event` | ✅ | not exposed (use `tracing` / `connection_events()`) |

A separate FFI shim crate (`epics-ca-rs-ffi`) is feasible but has not
been built. Consumers needing libca C ABI compatibility today should
link libca directly.

## Behavioural mismatches (deliberate)

A few cases where `epics-ca-rs` deliberately diverges from libca,
typically for safety or simplicity:

1. **Per-client subscriber mpsc bounded at 64 on the server**. libca's
   per-record event queue is a free-list with overflow tracking. We
   substitute a fixed bounded queue + coalesce slot which has
   equivalent observable behaviour but smaller code surface.

2. **`CaClient::drop` aborts tasks immediately**. libca has a graceful
   `ca_context_destroy` that waits for in-flight callbacks. Use
   `CaClient::shutdown().await` for graceful semantics.

3. **No `READ_BUILD` (cmd=16) or `SIGNAL` (cmd=25) handling**. Both
   are deprecated; modern clients don't emit them. Server replies
   with `CA_PROTO_ERROR` if it ever sees one.

An earlier revision of this list opened with a fourth item: a hard-pinned
`EPICS_CAS_USE_HOST_NAMES = NO` default, described as matching rsrv. That
was not a deliberate divergence but a mistake — no such variable exists
anywhere in epics-base, and rsrv's real default is the opposite: it trusts
the client-supplied hostname, and `asCheckClientIP` is the opt-in for IP
matching. The port now follows C in both modes (R7-16); see
`08-environment.md`.

4. **DBR_STSACK_STRING ackt/acks for SimplePv = 0**. libca returns the
   record's actual ACKT/ACKS; we substitute 0 for `SimplePv` (which
   has no record-level alarm-acknowledge state). Records work
   identically.

## Verification status

The interop test suite (`tests/interop_*.rs`) exercises the parity
matrix in both directions against a real C `softIoc`. Coverage:

- ✅ Rust client → C softIoc: caget, caput, camonitor with C IOC
- ✅ C tools (caget/caput/camonitor/cainfo) → Rust softioc-rs
- ✅ pyepics (libca-based) → Rust softioc-rs (when pyepics installed)
- ✅ Beacon-driven reconnect after IOC restart
- ✅ Concurrent channel storm, write burst, monitor backlog

Long-running soak (24 h+) is the user's responsibility via `ca-soak`;
infrastructure is documented in [`../TESTING.md`](../TESTING.md).
