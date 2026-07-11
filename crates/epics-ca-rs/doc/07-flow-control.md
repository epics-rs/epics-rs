# 07 — Flow control and backpressure

CA has flow control at two layers: a TCP-level pause/resume protocol
(`EVENTS_OFF` / `EVENTS_ON`) and per-subscription buffering with
"drop-oldest, keep-newest" coalescing. `epics-ca-rs` implements both
on both ends of the wire.

## Layer 1 — TCP-level flow control (EVENTS_OFF / ON)

### Protocol

The client sends `CA_PROTO_EVENTS_OFF` (cmd 8) to ask the server to
stop emitting any monitor updates. `CA_PROTO_EVENTS_ON` (cmd 9)
resumes. Both are payloadless headers.

The signal is **per virtual circuit** (per TCP connection), not per
subscription. While EVENTS_OFF is in effect, the server pauses every
subscription tied to that client.

### Client side: when do we send EVENTS_OFF?

Keyed on **OS socket-buffer occupancy**, never on how far behind the
application is — this is libca's rule verbatim (`tcpiiu.cpp:543-572`).

After each received frame is processed, `client/transport.rs::read_loop`
asks the OS whether unread bytes are *still* sitting in the socket
receive buffer (C `bytesArePendingInOS()`, an `ioctl(FIONREAD)`):

- **Bytes still pending** → increment `contig_recv_msg_count`. Once it
  reaches `protocol::max_contiguous_frames()`, the circuit is busy: send
  `EVENTS_OFF`.
- **Socket read clean** → reset `contig_recv_msg_count` to 0 and, if
  flow control is active, send `EVENTS_ON` *immediately*. C's comment:
  "if no bytes are pending then we must immediately switch off flow
  control w/o waiting for more data to arrive" (`tcpiiu.cpp:559-561`).

`max_contiguous_frames()` is C `cac.cpp:233-237`: the base trigger is
`contiguousMsgCountWhichTriggersFlowControl` = 10 (`iocinf.h:62`), scaled
by how many 16 KiB receive buffers one max-size array occupies, so a
circuit configured for large waveforms tolerates proportionally more
contiguous frames before tripping.

There is **no consumer-queue counter and no hysteresis**. libca has
neither, and a counter keyed on consumer backlog would let one
application that stops polling its `MonitorHandle` hold `EVENTS_OFF` down
for every *other* subscription on the same circuit — a state libca cannot
reach, because the moment the socket drains it emits `EVENTS_ON`.

Per-subscription buffering (the bounded channel plus the coalesce slot,
layer 2 below) is entirely separate and never reaches the wire.

### Server side: how do we honour it?

`server/monitor.rs::FlowControlGate`:

```rust
pub struct FlowControlGate {
    paused:  AtomicBool,
    resumed: Notify,
}
```

One instance per `ClientState` (i.e. per TCP circuit), held in an
`Arc` and shared by every monitor sender task on that circuit.

The TCP dispatch handler for `EVENTS_OFF` / `EVENTS_ON` calls
`pause()` / `resume()` (`tcp.rs:835`).

Each `spawn_monitor_sender` task checks `is_paused()` after every
recv. If paused, it enters a coalescing helper that keeps the most
recent event arriving on the mpsc and waits for either:

1. The mpsc to deliver a newer event (overwrites the saved one)
2. `resumed.notify_waiters()` → exit coalesce, send the saved event,
   continue the normal loop

This means under EVENTS_OFF the server stops writing to the TCP
socket but does not lose state — the client sees the **latest** value
when it resumes, not a backlog.

## Layer 2 — Per-subscription queue + coalesce slot

CA has no on-wire contract for "the server's per-subscription queue
overflowed" — that's an internal property of each end. Both
`epics-ca-rs` ends bound their per-subscription queues and use a
coalesce slot for drop-oldest semantics.

### Client side

`subscribe_with_deadband` (`client/mod.rs:758`) creates a bounded
mpsc:

```rust
let queue_size = epics_base_rs::runtime::env::get("EPICS_CA_MONITOR_QUEUE")
    .and_then(|s| s.parse::<usize>().ok())
    .unwrap_or(256)
    .max(8);
let (callback_tx, callback_rx) = mpsc::channel(queue_size);
```

When a `MonitorData` arrives, the coordinator calls
`subscriptions.on_monitor_data(...)`. That decodes the payload to a
`Snapshot` and does `try_send` (`subscription.rs:81`):

```rust
match rec.callback_tx.try_send(Ok(snapshot)) {
    Ok(())                 => MonitorDeliveryOutcome::Queued(server_addr),
    Err(TrySendError::Full(_))   => MonitorDeliveryOutcome::Dropped(server_addr),
    Err(TrySendError::Closed(_)) => MonitorDeliveryOutcome::Dropped(server_addr),
}
```

The coordinator increments `CaDiagnostics::dropped_monitors` on
`Dropped`. There is no client-side coalesce slot; the assumption is
that EVENTS_OFF will trigger before the queue fills repeatedly.

If your application needs lossless delivery, increase
`EPICS_CA_MONITOR_QUEUE` and ensure `MonitorHandle::recv` is called
in a tight loop.

### Server side

Per-PV subscriptions use mpsc(64) plus a slot:

```rust
pub struct Subscriber {
    pub sid: u32,
    pub data_type: DbFieldType,
    pub mask: u16,
    pub tx: mpsc::Sender<MonitorEvent>,
    pub coalesced: Arc<StdMutex<Option<MonitorEvent>>>,
}
```

Producer (record processing → `notify_subscribers`):

```rust
match sub.tx.try_send(event.clone()) {
    Ok(())  => {}                                      // queued
    Err(_)  => {
        if let Ok(mut slot) = sub.coalesced.lock() {
            *slot = Some(event);                        // overwrite prior overflow
        }
    }
}
```

Consumer (`spawn_monitor_sender`):

```rust
loop {
    let next = if let Some(ev) = pv.pop_coalesced(sub_id).await {
        Some(ev)               // drain coalesce slot first
    } else {
        rx.recv().await        // then the mpsc
    };
    // ... encode + write ...
}
```

The same pattern applies to `RecordInstance::pop_coalesced` and the
inline RecordField task in `tcp.rs:817`.

This guarantees the **most recent value is always delivered**, even
under sustained producer overload, at the cost of intermediate values
being dropped.

## Send-side backpressure (transport)

Distinct from the EVENTS flow control above: the client transport
also caps its outgoing TCP write queue.

`client/transport.rs`:

```rust
const SEND_BACKPRESSURE_FRAMES: usize = 4096;

if pending_frames >= SEND_BACKPRESSURE_FRAMES {
    eprintln!("CA: {server_addr}: send buffer stalled, closing");
    // drop the connection — let the coordinator retry from scratch
}
```

`pending_frames` counts the number of frames sitting between the
write_tx mpsc and the OS socket buffer. When it climbs above 4096 the
write task is stalled (TCP write stuck) and we close the connection
rather than letting the queue grow without bound. This matches libca
`flushBlockThreshold` semantics.

The write loop also wraps `writer.write_all` in a 10-second timeout
(`SEND_TIMEOUT = 2 × ECHO_TIMEOUT_SECS`). If a TCP write hangs that
long, the connection is declared dead.

## Producer rate limiting (search engine AIMD)

A separate kind of "flow control" applies to UDP search:

```
state.budget.frames_per_try
    starts at MAX_FRAMES_PER_TRY (50)
    +1 on >50% response rate in a 1-second window  (additive increase)
    →1 on <10% response rate                        (multiplicative decrease)
```

This caps the number of UDP datagrams the engine sends per second
when many channels are searching simultaneously, avoiding overwhelming
intermediate switches. Source: `client/search.rs::SendBudget::evaluate`.

## Inactivity / liveness watchdogs

Not flow control per se, but related — they bound how long
unresponsive ends can pin resources:

| Layer | Watchdog | Default |
|-------|----------|---------|
| Client TCP | echo idle timeout | 30 s (`EPICS_CA_CONN_TMO`) |
| Client TCP | echo response timeout | 5 s |
| Client TCP | send watchdog | 10 s (2 × echo) |
| Server TCP | inactivity timeout | 600 s (`EPICS_CAS_INACTIVITY_TMO`) |
| Server TCP | OS keepalive | 15 s idle / 5 s probe |
| Beacon monitor | re-register interval | 5 min |

When any watchdog fires, the affected connection is closed, which
funnels into the disconnect → re-search → reconnect path documented
in [`05-state-machines.md`](05-state-machines.md).

## Tuning summary

For a high-throughput consumer (many monitors at high rates):

```bash
EPICS_CA_MONITOR_QUEUE=2048           # bigger client queue
EPICS_CA_MAX_SEARCH_PERIOD=60         # faster search recovery
```

For a server expecting many slow clients:

```bash
EPICS_CAS_MAX_CHANNELS=16384          # opt into a per-client channel cap (default: unbounded)
EPICS_CAS_INACTIVITY_TMO=300          # tighter idle cap
EPICS_CAS_BEACON_PERIOD=15            # default
```

For a write-heavy, low-monitor workload:

```bash
# defaults are fine; the bottleneck is record processing not CA
```

For diagnostics:

```bash
# inspect at runtime via CaClient::diagnostics()
# or at the end of a soak with `ca-soak`
```
