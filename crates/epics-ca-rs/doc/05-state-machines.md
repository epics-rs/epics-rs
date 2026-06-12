# 05 — State machines and lifecycles

This document describes the behavioural contracts of the CA client and
server in terms of state transitions and message sequences. It
complements the structural docs ([`03-client.md`](03-client.md),
[`04-server.md`](04-server.md)).

## Channel lifecycle (client)

```
                       ┌────────────┐
   create_channel() ──▶│ Searching  │
                       └─────┬──────┘
                             │ SearchResponse::Found
                             ▼
                       ┌────────────┐
                       │ Connecting │
                       └─────┬──────┘
                             │ TransportEvent::ChannelCreated
                             ▼
                       ┌────────────┐  echo timeout (1st miss)
                       │ Connected  │ ─────────────────┐
                       └─────┬──────┘                  ▼
                             │                  ┌──────────────┐
                             │                  │ Unresponsive │
                             │                  └──────┬───────┘
                             │                         │ data received
                             │ ◀───────────────────────┘
                             │
   ┌─────────────────────────┼─────────────────────────┐
   │                         │                         │
   │ TcpClosed               │ ServerDisconnect        │ DropChannel (user)
   ▼                         ▼                         ▼
┌──────────────┐       ┌──────────────┐         ┌──────────────┐
│ Disconnected │       │ Disconnected │         │   Shutdown   │
└──────┬───────┘       └──────┬───────┘         └──────────────┘
       │  re-search                              (terminal — channel removed)
       │
       └─▶ Searching ─▶ Connecting ─▶ Connected (subscriptions auto-restored)
```

Source: `client/state.rs::ChannelState`, plus the transitions in
`client/mod.rs::run_coordinator` (`mod.rs:858`).

### Properties

- `state.is_operational() == matches!(state, Connected | Unresponsive)`.
  IO operations require this.
- `Searching → Connecting → Connected` on first connect.
- `Connected → Unresponsive → Connected` is reversible: when data
  arrives, the read loop emits `CircuitResponsive`.
- Disconnect always re-searches, never gives up. Backoff via
  `reconnect_count` (lane 1..8).
- `Shutdown` is a terminal state set when the user drops the channel;
  the coordinator removes the entry shortly after.

### Reconnection backoff

`handle_disconnect` (`mod.rs:1338`):

```
sustained = (now - last_connected_at) > 30s
if sustained:
    reconnect_count = 0
else:
    reconnect_count += 1
initial_lane = clamp(reconnect_count, 1, 8)
SearchRequest::Schedule { ..., initial_lane }
```

The search engine uses `lane_period(lane) = base_rtte << lane`, capped
at `EPICS_CA_MAX_SEARCH_PERIOD`. Jitter of 0..50% of the period is
applied so a fleet of clients reconnecting after a server restart
doesn't dogpile.

This matches the C `disconnectGovernorTimer` policy.

## Search lifecycle

```
              ┌──────────────────────┐
              │  Schedule (initial)  │
              └──────────┬───────────┘
                         │ inserted with lane_index=initial_lane
                         ▼
              ┌──────────────────────┐
              │  in deadline_set     │◀──┐
              └──────────┬───────────┘   │ no response
                         │               │ → lane_index += 1
                         │ deadline      │ → reschedule
                         ▼               │
              ┌──────────────────────┐   │
              │  send_due_searches   │───┘
              │   (UDP+nameserver)   │
              └──────────┬───────────┘
                         │
                         │ SEARCH response
                         ▼
              ┌──────────────────────┐
              │  remove_channel      │ → emit Found
              └──────────────────────┘
```

Sources:
- Initial: `mod.rs::create_channel` →
  `SearchRequest::Schedule { initial_lane: 0 }`
- Reconnect: `handle_disconnect` →
  `SearchRequest::Schedule { initial_lane: 1..8 }`
- Beacon anomaly: `ForceRescanServer` →
  `SearchRequest::Schedule { initial_lane: 0, reason: BeaconAnomaly }`
  (also enables a 5-second fast-rescan window)

## Connect sequence (client + server, success path)

```
Client                                    Server
──────                                    ──────
create_channel(cid)                       (idle)
SearchRequest::Schedule
   │
   ▼ UDP broadcast
SEARCH(cid, name)                ─────▶  parse, db.has_name? yes
                                          local_ip_for(src)
                                          build VERSION + SEARCH reply
SEARCH reply (cid → server_ip)   ◀─────  unicast UDP
   │
   ▼ Found
TransportCommand::CreateChannel
   │
   ▼ TCP connect to server_addr
TcpStream::connect              ─────▶   accept(), keepalive on
VERSION+HOST+CLIENT_NAME         ─────▶  cache client_minor_version
                                          state.hostname = peer.ip()
                                          (or client-supplied if env)
CREATE_CHAN(cid, pvname)         ─────▶  alloc sid, look up PV
                                          send ACCESS_RIGHTS, CREATE_CHAN
                                          insert ChannelEntry
ACCESS_RIGHTS(cid, bits)         ◀─────
CREATE_CHAN(cid, sid, type, n)   ◀─────
   │
   ▼ ChannelCreated event
ChannelInner.state = Connected
broadcast ConnectionEvent::Connected
broadcast AccessRightsChanged
restore_for_channel(cid, sid)
   │  (re-issue EVENT_ADD for each
   │   live subscription — no-op on
   │   first connect)
```

## Read sequence

```
Client                                    Server
──────                                    ──────
ch.get_with_timeout(t)
   │
   ▼ CoordRequest::GetChannelInfo
   │ → snapshot {sid, native_type, ...}
   ▼
CoordRequest::ReadNotify { ioid }
TransportCommand::ReadNotify
READ_NOTIFY(sid, dbr, n, ioid)   ─────▶  channel_access[sid] check
                                          encode_dbr(snapshot)
READ_NOTIFY(ECA_NORMAL, payload) ◀─────
   │
   ▼ TransportEvent::ReadResponse
   │ read_waiters.remove(ioid)
   │ waiter.send(Ok(...))
   ▼
ch.get_with_timeout returns Ok((dtype, value))
```

On an error path the server sends `READ_NOTIFY` with `cid =
ECA_BADCHID` (or similar) and an empty payload. The client emits
`TransportEvent::ReadError { ioid, eca_status }` and the waiter gets
`Err(CaError::Protocol(...))`.

## Write sequence

Identical structure with `WRITE_NOTIFY` replacing `READ_NOTIFY`.
Special cases:

- `put_nowait` uses `CA_PROTO_WRITE` (cmd=4). Server does **not** send
  any reply; the client doesn't allocate a write_waiter.
- `put` with `data_type ∈ {DBR_PUT_ACKT, DBR_PUT_ACKS}` is routed by
  the server to the record's ACKT/ACKS field via
  `db.put_record_field_from_ca` — never to the channel's normal write
  path.

## Subscribe sequence

```
Client                                    Server
──────                                    ──────
ch.subscribe()
   │
   ▼ CoordRequest::Subscribe
   │ subscriptions.add(...)
   │ if connected: TransportCommand::Subscribe
   │ else: needs_restore=true (will be sent after reconnect)
   ▼
EVENT_ADD(sid, dbr, n, sub_id, mask) ──▶ alloc spawn_monitor_sender
                                         pv.add_subscriber → mpsc(64)+slot
                                         send initial snapshot
EVENT_ADD(ECA_NORMAL, payload)     ◀──── (initial)
   │
   │ ... time passes ...                  pv.set(...) → notify_subscribers
EVENT_ADD(ECA_NORMAL, payload)     ◀──── (per change)

ch.unsubscribe (drop MonitorHandle)
   │
   ▼ CoordRequest::Unsubscribe
   ▼ TransportCommand::Unsubscribe
EVENT_CANCEL(sid, sub_id)          ────▶ remove subscription
EVENT_ADD(count=0)                  ◀──── (final reply per spec)
```

### Coalescing under load

If the producer (record processing) outpaces the consumer (TCP write),
events queue in mpsc(64). When that fills, libca-style "drop-oldest,
keep-newest" semantics kick in:

```
producer notify_subscribers:
    match sub.tx.try_send(event):
        Ok(_)  → done
        Full(_)→ *sub.coalesced.lock() = Some(event)   // overwrites prior overflow

consumer spawn_monitor_sender:
    loop:
        if let Some(ev) = pv.pop_coalesced(sub_id).await:
            send(ev); continue
        rx.recv().await | send
```

This guarantees the **most recent value is always delivered**, at the
cost of intermediate values being dropped under sustained overload.

## EVENTS_OFF / EVENTS_ON (TCP-level flow control)

Both client and server participate. Trigger conditions on the client:

```
flow_control[server_addr].outstanding >= FLOW_CONTROL_OFF_THRESHOLD (10)
    → send EVENTS_OFF
flow_control[server_addr].outstanding <= FLOW_CONTROL_ON_THRESHOLD (5)
    → send EVENTS_ON
```

The "outstanding" count is per server, summed across all subscriptions
on that server. Incremented when a `MonitorData` is delivered to the
application, decremented when the user calls `MonitorHandle::recv`
(via `CoordRequest::MonitorConsumed`).

On the server, `EVENTS_OFF` calls `FlowControlGate::pause()`. While
paused, the per-subscription `spawn_monitor_sender` task keeps
draining its mpsc but coalesces every event into the most recent
(`coalesce_while_paused`). On `EVENTS_ON` (`resume()`), the most
recent value is delivered.

The combined effect: under EVENTS_OFF the server's TCP write rate goes
to zero, but no events are lost from the perspective of "the next
delivered value will be current". This matches libca behaviour.

## Beacon anomaly path

```
Client                                    Server
──────                                    ──────
                                          (server starts up after restart)
                                          beacon_id = 0..1..2..  fast ramp 20ms→15s
                                          UDP broadcast to 5065
            ┌─────────────────────────┐
            │ local CA repeater       │ ◀── beacon
            │ fan-out to subscribers  │
            └────────┬────────────────┘
                     │
                     ▼
              beacon_monitor task
                     │
                     │  beacon_id != last_id+1 OR period < estimate/3
                     ▼
              CoordRequest::ForceRescanServer
                     │
                     ▼
   coordinator:
     diag.beacon_anomalies++
     for each channel in (Disconnected | Searching):
        SearchRequest::Schedule { reason: BeaconAnomaly, lane: 0 }
     for each operational channel on any server:
        TransportCommand::EchoProbe
            │
            ▼ via transport.read_loop
   read_loop wakes, sends ECHO,
   enters echo_pending mode (5 s timeout)
   if no data → TcpClosed → handle_disconnect
   else → CircuitResponsive
```

Net effect: an IOC restart is detected within tens of milliseconds via
the fast beacon ramp; affected channels are re-searched immediately,
and any TCP circuits that survived but pointed at the old IOC are
killed within the 5-second echo window.

## Disconnection paths

There are three ways a circuit can die. All converge on
`handle_disconnect`:

1. **TCP error** (read returns 0 or Err) → `TcpClosed` event.
2. **Server-initiated single-channel disconnect** (`SERVER_DISCONN`
   opcode) → only the named cid moves to Disconnected; other channels
   on the same TCP circuit stay alive.
3. **Echo timeout x2** → `TcpClosed`. The first miss emits
   `CircuitUnresponsive` (channels marked Unresponsive but still
   operational); the second confirms the circuit is dead.

`handle_disconnect` always:

- Resets per-server flow control state
- Marks affected channels as Disconnected, broadcasts events
- Resets `pending_deliveries` on each affected subscription
- Schedules re-search with backoff
- Fails in-flight `read_waiters` and `write_waiters` with
  `CaError::Disconnected`

Code: `client/mod.rs:1338`.

## Server-side connection lifecycle

```
TCP listener.accept()
     │
     ▼ enable keepalive(15/5/3)
     ▼ broadcast ServerConnectionEvent::Connected(peer)
     ▼
spawn handle_client(stream, peer)
     │
     │  inactivity timeout: read with EPICS_CAS_INACTIVITY_TMO
     │  accumulated buffer: capped at 1 MB
     │
     │  CREATE_CHAN check: < EPICS_CAS_MAX_CHANNELS    (only when set)
     │  EVENT_ADD check:   < EPICS_CAS_MAX_SUBS_PER_CHAN (only when set)
     │
     ▼
loop: read frame → dispatch → reply
     │
     ▼ on EOF / error / inactivity / overflow
cleanup:
   for each subscription: task.abort(); pv.remove_subscriber
broadcast ServerConnectionEvent::Disconnected(peer)
beacon_reset.notify_one()    ← so clients see departure quickly
```

## Reasoning about correctness

A few invariants worth explicitly stating:

1. **Coordinator ownership**: only the coordinator task mutates
   `channels`, `subscriptions`, `read_waiters`, `write_waiters`,
   `flow_control`. There is no shared-mutable state between client
   tasks; all coordination is via mpsc/oneshot.

2. **Bounded memory**: every per-channel structure has a fixed
   upper bound. Per-subscription mpsc is bounded (256). Per-server
   write queue is bounded (4096 frames). Diagnostic history is bounded
   (256 events). There is no growth path on long-running soak.

3. **In-flight failure**: every operation that produces a future
   (read/write/subscribe/wait_connected) returns `Err(Disconnected)`
   or `Err(Timeout)` rather than hanging. This is enforced at
   `handle_disconnect` for read/write_waiters; subscriptions emit no
   events while disconnected.

4. **Reconnection idempotency**: re-search after disconnect uses the
   same `cid`. The new `sid` from CREATE_CHAN replaces the old one in
   `ChannelInner.sid`. Subscription restore uses the new sid.
   Outstanding `ioid` from the disconnected period are aborted, so
   there's no risk of an old reply being misrouted to a fresh
   waiter.

5. **No leaks across IOC restart**: `ManagedIoc::Drop` (test harness)
   plus `pv.remove_subscriber` in cleanup ensures both sides release
   resources promptly. Observable as `Disconnections == Reconnections`
   in `CaDiagnostics` after a soak run.

These invariants are exercised by the `stress_load` and
`interop_rust_client_c_ioc` test suites (see
[`../TESTING.md`](../TESTING.md)).
