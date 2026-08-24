# 06 — State machines

## Channel lifecycle (client side)

`client_native/channel.rs::ChannelState`:

```text
                 ┌────────┐
                 │  Idle  │ ← ChannelState::Idle (just constructed)
                 └────┬───┘
                      │ ensure_active() (any op call)
                      ▼
                 ┌────────────┐    SearchEngine resolves PV name
                 │ Searching  │── (UDP / nameserver) ─┐
                 └────────────┘                       │
                                                      ▼
                 ┌────────────┐    ConnectionPool.get_or_connect(addr)
                 │ Connecting │── runs handshake ─────┐
                 └────────────┘                       │
                                                      ▼
                 ┌──────────────────────┐   ServerConn.is_alive()
                 │ Active{server, sid}  │── = false ──┐
                 └──────────────────────┘             │
                          ▲                           │
                          │  ServerConn re-established│
                          │                           │
                          └─────── Reconnecting ◀─────┘
```

Transitions are serialized by `Channel::transition_lock` so two
concurrent ops cannot trip `Connecting` twice.

### Holdoff

After a pass where every candidate failed, `reconnect_holdoff` picks the
delay from the failure class and the resolver kind — there is no ladder
across consecutive failures:

```text
searched + TCP failure        → SEARCH_RING_HOLDOFF      (10s)
searched + CREATE refusal     → none (≤1s ring tick paces it)
direct   + TCP failure        → DIRECT_RECONNECT_HOLDOFF  (2s)
direct   + CREATE refusal     → DIRECT_REFUSAL_HOLDOFF   (10s)
```

While `holdoff_until > now`, `ensure_active()` sleeps out the remainder
before its next attempt. A successful `Active` transition clears it.

## Search backoff

`SearchEngine::send_due_searches` walks every pending PV and decides
whether to retransmit based on the per-PV state:

```text
┌────────────┐ initial / reset
│ lane 0     │── 1s deadline
└────────────┘
                 ▼ (no response)
┌────────────┐
│ lane 1     │── 1s deadline
└────────────┘
                 ▼
┌────────────┐
│ lane 2     │── 2s
└────────────┘
                 ▼ ...
┌────────────┐
│ lane 9     │── 210s (cap)
└────────────┘
                 ▼ stays at lane 9 forever
```

`hurry_up()` resets every pending entry to lane 0 and triggers an
immediate search round. Mirrors pvxs `Context::hurryUp`.

Beacon-driven fast reconnect: when the beacon listener observes a
new GUID for a server we have channels on, every channel for that
server jumps back to lane 0 immediately. Mirrors pvxs
`BeaconTracker` + `Channel::poke`.

## Monitor lifecycle (one ioid, one direction)

```text
client                                    server
──────                                    ──────
                          INIT (pvRequest)
   ───────────────────────────────────────▶
                          INIT response (status + introspection)
   ◀───────────────────────────────────────
                          START
   ───────────────────────────────────────▶
                                              spawn subscriber task

                          DATA (bitset + values)
   ◀───────────────────────────────────────
                          ...
                          DATA
   ◀───────────────────────────────────────
                          DESTROY_REQUEST       (or CANCEL_REQUEST to pause)
   ───────────────────────────────────────▶
                                              drop OpState → abort task

                          FINISH (subcmd 0x10)  (only when source ends)
   ◀───────────────────────────────────────
```

START / pipeline-ack uses subcmd `0x40` in the formal spec, `0x80`
in the legacy "pipeline" variant pvxs and spirit emit. `pva-rs`
treats either as "start emitting / ack window". Plain `0x00` is
also accepted for backward compat.

## Reconnection (monitor, client side)

`op_monitor` re-issues INIT/START on every reconnect:

```text
loop {
    channel.ensure_active().await       // blocks on holdoff if any
    server.send(MONITOR_INIT(ioid, mask))
    await INIT response
    server.send(MONITOR_START(ioid))
    loop {
        match server.recv_for_ioid(ioid).await {
            Ok(frame) => callback(decode(frame)),
            Err(disconnect) => break,        // exit inner loop
        }
    }
    // outer loop iterates → ensure_active() again
}
```

Net effect: the user-facing callback (`MonitorEvent::Data` / typed
event stream) survives any number of reconnects without the caller
caring. Optional `MonitorEvent::Disconnected` and `MonitorEvent::Connected`
events surface the boundaries when `MonitorEventMask` doesn't mask them.

## Per-connection liveness (client side)

```text
ServerConn reader task:
    on read(n>0):  last_rx_nanos = now()
    on EOF / err:  set alive=false, cancel.cancel()

ServerConn heartbeat task:
    every 15s:
        if (now - last_rx_nanos) > 30s {
            warn!(...); break
        }
        send ECHO_REQUEST control
```

Once `cancel.cancel()` fires, every awaiter on `cancel.cancelled()`
unblocks and the writer task exits. `Channel::is_alive()` → false →
the next op transitions back through `Searching`.

## Per-connection liveness (server side)

```text
TCP read loop:
    every 15s tick (heartbeat task):
        if idle > op_timeout {
            warn!(...); break
        }
        send ECHO_REQUEST
    on EchoRequest received:  echo back

    on read(n>0):  last_rx = now
    on read(n=0): break
    on op error:  break

cleanup on break:
    drop channels HashMap → drops every monitor_abort → tasks aborted
    audit("disconnect", ...)
```
