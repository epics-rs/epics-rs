# What bounds a CA client that stops reading

Measured 2026-08-03. Four runs: three against the live `caioc-deploy.exe` guest
on the RTEMS/QEMU rig (`xilinx-zynq-a9`, 256 MB, hostfwd `tcp:127.0.0.1:5064`)
and the `realtime-ca-ioc.vxe` RTP on the VxWorks 7 E8 guest, both of which run
`server/blocking.rs`; one against `softioc-rs` on loopback, which runs
`server/tcp.rs`. The two drivers do not answer the memory question the same way
— see [The outbox](#the-outbox-the-two-drivers-do-not-share-the-exposure).

## The claim under test

`server/blocking.rs::handle_client_blocking` used to justify its
`SO_KEEPALIVE` call this way:

> KEEPALIVE is what bounds a client that stops reading: `write_frame_locked`
> parks in `write_all` under the send lock, exactly as C parks in `send` under
> `SEND_LOCK`, and in C it is the keepalive probe failing that ends it.

The first clause is false, and the rest of the sentence is what made it
plausible: the parking half is exactly right, so the reaper half went
unchecked.

## Why the stack says it is false

Read from the rig's rtems-libbsd tree, `freebsd/sys/netinet/`:

1. A peer that stops reading advertises a **zero window**. That runs the
   **persist** timer, not the keepalive timer.
2. Keepalive is rearmed by inbound traffic (`tcp_input.c:2061`, `:2460`), and a
   zero-window peer keeps ACKing the window probes, so keepalive never expires
   in the first place.
3. The persist timer's own drop needs
   `tp->t_rxtshift == TCP_MAXRXTSHIFT && (ticks - tp->t_rcvtime >=
   tcp_maxpersistidle || ...)` (`tcp_timer.c:540-541`). But
   `tp->t_rcvtime = ticks` is refreshed by **every** inbound segment
   (`tcp_input.c:1596`) — and those probe ACKs are inbound segments. A peer
   that answers probes and never reads refreshes the idle clock forever.
4. `tcp_maxpersistidle = TCPTV_KEEP_IDLE` (`tcp_subr.c:1098`). The two timers
   are configured from the same constant, which is the likeliest source of the
   original mix-up.

Prediction: nothing reaps such a client, and the server thread serving it stays
parked in `write` under the send lock indefinitely.

## The experiment

[`ca-stuck-reader/stuck_reader.py`](ca-stuck-reader/stuck_reader.py) opens two
raw CA connections to the guest and never touches the IOC's data. Every run's
script and raw output is in [`ca-stuck-reader/`](ca-stuck-reader/).

* **A** — `SO_RCVBUF` 2048 set before `connect`, full CA handshake, one channel
  on `RTEMS:MEM_FREE`, then 50,000 pipelined `CA_PROTO_READ_NOTIFY` requests
  and **no reads at all** — intended to overwhelm A's window so the server's
  write for A parks. See the caveat under Result for how far that intent is
  actually established.
* **B** — an ordinary client opened every 30 s: handshake, channel, one
  `READ_NOTIFY`, read the reply, close. B answers the question that actually
  matters operationally — does one stuck reader wedge the whole IOC, or only
  its own CAS thread?

## Result

Held for 1800 s (30 minutes), sampled every 30 s.

* **A was never dropped.** Still `connected` at every sample and at the final
  check. Persist backoff reaches `TCP_MAXRXTSHIFT` well inside a 30-minute
  window, so had `t_rcvtime` not been refreshed the drop would have landed
  inside it.

  One caveat, stated because it bounds the claim: QEMU's SLIRP terminates TCP,
  so the connection the guest actually sees is guest↔SLIRP, and it is SLIRP's
  window that had to shut for the guest to enter persist. A's 50,000 requests
  produce roughly 2 MB of replies against SLIRP's ~64 KB per-socket buffering,
  which is far past the point where the guest's window must close — but that
  was inferred, not read off the guest.
* **B was served at every one of the 60 samples.** 59 of them in 0.01 s; the
  single exception is the first (`t+30s`, 0.43 s), taken while A's 50,000
  requests were still being dispatched. From `t+60s` on it is 0.01 s flat. Not
  merely served — served with no measurable added latency. The wedge is
  strictly per-client.

## What follows

* The behaviour is **C-faithful and is left alone**. C parks in `send` under
  `SEND_LOCK` for exactly as long; `cas_send_bs_msg` has no timeout either.
  Only the rationale in the comment was wrong, and only the comment changed.
* The one bound that exists is `EPICS_CAS_INACTIVITY_TMO` (`SO_RCVTIMEO` on the
  read side), which is **off by default**, as in C.
* `SO_KEEPALIVE` is still set, and still worth setting — it reaps a peer that
  goes silent with a *non-zero* window (a vanished host, a stalled TLS
  ClientHello). That is a different scenario from this one, and the five other
  keepalive-reaps-it comments in the workspace all describe that scenario and
  are correct.

## The hosted driver has no stall guard either — and its comments said otherwise

Found while checking that the 15 s ENOBUFS backoff added in
`server::send` could not be cut short by a send timeout. It cannot, and the
reason is the finding.

`tcp.rs:1484` sets `SO_SNDTIMEO`, and its own comment correctly notes the
kernel does not apply it to a non-blocking tokio socket, so "a stuck client
where the kernel send buffer fills would still leave `poll_write` Pending
forever." It then named a replacement:

> The actual stall guard is the `tokio::time::timeout` wrapping
> `dispatch_message` in `handle_client`'s read loop.

That guard cannot fire on a socket stall. `dispatch_message` takes
`writer: &Outbox` and never touches `sock` — the batch-flush refactor moved
every socket write out to `drain_and_flush` at the bottom of the loop, which is
**not** wrapped in a timeout. The comment at the `timeout` call site
(`tcp.rs:2128`) stated the same old purpose, "so a stuck-reader client (kernel
send buffer full → `write_all` Pending forever) can be detected", while two
paragraphs below it acknowledged the refactor that had removed it.

Both comments were corrected in `c388afe4`; no code changed with them. Whether
to restore a real bound is the open decision recorded at the bottom of this
document.

## The outbox: the two drivers do NOT share the exposure

An earlier draft of this document asserted the opposite:

> Both drivers share the exposure: `blocking.rs::drain_outbox_locked` parks under
> the send lock while the event worker keeps pushing into the same unbounded
> queue.

That is wrong. The event worker never touches the outbox, and the two drivers
are bounded and unbounded respectively — by construction, not by accident.

**Blocking driver — bounded.** `blocking.rs:1218-1219` gives the CAS-event
thread its writer as
`let mut write = |frame: &[u8]| write_frame_locked(&event_send_lock, frame);`.
It writes the socket *directly*, under the same `send_lock` the read/dispatch
thread drains its command replies under. Only a subscription's initial snapshot
is queued in `outbox` (`:1385`); every subsequent update goes down that closure.
So when the read thread parks in `write` holding `send_lock`, the event thread
blocks on the **mutex**, stops pulling from its `EvQue`, and the back-pressure
lands in that bounded ring — where a post arriving with no room replaces this
monitor's last entry in place. This is C's structure: `client->lock` serializes
`camsgtask` and `event_task` (`server.h:221`) in front of a bounded `dbEvent`
ring.

**Hosted driver — was unbounded.** Fixed below; this is what the measurement
found. `monitor.rs:178` ended the monitor task with
`outbox.push(frame.seal(&hdr))`, a synchronous send into
`mpsc::unbounded_channel()` that never blocks and is decoupled from the socket.
Back-pressure never reached the `EvQue`, so that ring always drained and never
coalesced, and the growth moved into the queue instead. The channel's own doc
justified the unbounded choice with "the sole draining owner pulls the queue
empty after [...]", which stops being true exactly when the drain parks.
Pipelined requests cannot exercise this — once the drain parks the
read loop stops dispatching, so no new replies are produced. It takes an
asynchronous producer, i.e. a subscription.

### Run 3 — blocking driver, on target: flat for 20 minutes

[`ca-stuck-reader/stuck_both.py`](ca-stuck-reader/stuck_both.py),
[log](ca-stuck-reader/stuck-both-1200.log). One connection does both halves:
subscribe to `RTEMS:MEM_FREE` and read 3 real events (proving the producer is
live at the measured 1.10 events/s), *then* flood 50,000 pipelined
`READ_NOTIFY` — ~1,953 KB of replies against SLIRP's ~64 KB of per-socket
buffering, so the guest's window must close — then read nothing for 1200 s.

Free memory took **three** values in 40 samples: 0 at t+30, −8,960 B from t+60
(the flood's own cost), −9,176 B from t+420, and nothing after. The second half
of the run moved **0 B**. At 1.10 events/s × 1200 s ≈ 1,320 frames, an
unbounded queue would have shown roughly 20 KB. Closing A returned free memory
to 12,344 B *above* the baseline.

### Run 4 — hosted driver, on loopback: linear, 32.8 MB/hour

[`ca-stuck-reader/hosted_growth.py`](ca-stuck-reader/hosted_growth.py),
[log](ca-stuck-reader/hosted-growth-600.log). The hosted driver is
`#[cfg(not(epics_embedded_target))]`, so it cannot be run on the rig at all;
this is `softioc-rs --record ai:DRV:0.0` on loopback, driven by a 100 Hz putter
(measured 98.2 events/s), with `ss` reading the server's own socket queues.

Loopback removes the confound that beat runs 1-3: QEMU's SLIRP terminates TCP,
so the queues visible from the host were SLIRP's, not the guest's. Here they
are the server's, which makes the parking a **direct observation** rather than
an inference from free memory:

* `Send-Q` = 1,357,200 B, **identical in all 60 samples** across 591 s. The
  drain is parked in `poll_write` for the whole window.
* `Recv-Q` = 45,184 B, identical in all 60. The read loop stopped consuming.
* `VmRSS` +648 kB → +6,168 kB over the same window: **9.34 kB/s**, linear, no
  knee. That is 32.8 MB/hour, 787 MB/day, from one stuck subscriber.

9.34 kB/s ÷ 98.2 events/s ≈ **96 B per queued frame** — a 24-byte
`CA_PROTO_EVENT_ADD` (16 B header, 8 B `DBR_DOUBLE`) plus its `Vec` allocation
and `mpsc` node. The arithmetic closes, so the growth is the outbox and not
something else in the process.

### Runs 2 and 1, in light of runs 3 and 4

[`ca-stuck-reader/stuck_monitor.py`](ca-stuck-reader/stuck_monitor.py).
A subscribed to `RTEMS:MEM_FREE` (measured producer rate 1.10 events/s) with a
2 KB receive buffer and read nothing for 1200 s, while B sampled `MEM_FREE`
every 30 s. Guest free memory took exactly two values across the whole run:
−10,664 B for the first 13 samples, then −10,880 B for the remaining 27. One
216-byte step, then flat for 810 s.

**That was read at the time as evidence of nothing.** 1.10 events/s × 1200 s ≈
1,320 frames of roughly 50 B ≈ 66 KB. If the drain had been parked, that is what
would have accumulated. 216 B did — so either nothing queues, or the frames left
the server and the write path never parked. QEMU's SLIRP hostfwd and the host
socket buffers sit between A and the guest and absorb 66 KB without complaint,
which makes the second reading entirely possible, and the run could not
distinguish them.

Run 3 supplies the flood run 2 lacked and reaches the same flat result, so run
2's number was right even though its conditions were never established. Read
now, both are the blocking driver behaving as `blocking.rs:1219` says it must.

The same confound bounds what run 1 proved. A was demonstrably never dropped
and B was demonstrably never delayed — both end-to-end observations, and both
hold. Whether the server's `write` was parked during those 30 minutes was not
independently verified; run 4 verifies it directly, but for the other driver.

**Disclosure about the ENOBUFS change.** The dominant stuck-reader case (zero
window, `poll_write` Pending) was already unbounded and unguarded before that
change; ENOBUFS previously returned `Err` and disconnected the client. So the
fix adds a second, rarer way to park, on a path that was already open. It does
not create the exposure, and it does lengthen it by up to 15 s per ENOBUFS.

## VxWorks 7: the same shape, with a much longer dispatch transient

[`ca-stuck-reader/stuck_reader_vx.py`](ca-stuck-reader/stuck_reader_vx.py),
[log](ca-stuck-reader/vx-stuck-reader-1800.log). Run 1's experiment against the
`realtime-ca-ioc.vxe` RTP on the E8 QEMU guest, 1800 s, sampled every 30 s.

* **A was never dropped** — `connected` at all 60 samples and at the final
  check, matching RTEMS.
* **B was served at 58 of 60 samples**, all in 0.02 s. The two exceptions are
  the first two: a 15.02 s connect timeout at t+30 and no `CREATE_CHAN` reply
  within 10.33 s at t+75. From t+115 on it is 0.02 s flat.

So the divergence from RTEMS is confined to the flood's dispatch transient, and
it is a difference of degree: dispatching 50,000 pipelined requests costs RTEMS
one sample at 0.43 s and costs VxWorks roughly 90 s during which a new client
cannot complete a handshake. After that the isolation is per-client on both
targets. No claim is made about why the transient is 3× longer.

The stack reasoning above is **not** transferred: the VxWorks 7 SDK on the rig
ships headers only — no TCP stack source — so the persist/keepalive argument
has not been checked there. The end-to-end result is what this run establishes.

## The fix: credit, and the re-measurement

The C-faithful answer was already in the tree — the blocking driver's — so the
question was only how to get the same back-pressure into the hosted driver
without giving up what the outbox bought. Two obvious routes are both wrong:

1. **Bound the channel.** Swapping `unbounded_channel` for a bounded one
   deadlocks: the read loop both pushes command replies into the outbox and is
   its sole drain, so a full queue has it await itself.
2. **Give the monitor task the socket, as `blocking.rs` does.** That is the
   structure that measures flat, but `monitor.rs` records that the outbox was
   adopted *to remove* an abort-safety hazard — under the former shared
   `Arc<Mutex<BufWriter>>` a `task.abort()` between header and payload could
   expose a partial frame, which one synchronous `push` makes impossible.
   Reverting reintroduces it.

Neither tension is real once the bound is put on the *producer* instead of on
the queue. `server/outbox.rs` now carries a second invariant:

> A producer that is not the connection loop MUST hold a `Credit` for every
> frame it enqueues, and only the drain owner may release one — by dropping the
> queued frame after its bytes are in the socket writer.

`Credit` is an `OwnedSemaphorePermit` (`MONITOR_CREDIT` = 64 per connection)
that rides *inside* the queued frame, so releasing it is the drain owner's
`Drop` — the same `Drop` that already returns the send buffer to the
`FramePool`. Accounting is symmetric and has one owner on each side: the
producer takes a credit only when it really enqueues, the drain returns one
only when it really wrote. Request→reply handlers pass `Credit::none()` and are
unaffected, which is what keeps route 1's deadlock off the table; the frame is
still sealed and enqueued in a single synchronous send with no await between
header and payload, which is what keeps route 2's hazard closed.

Credit is taken *after* the event leaves the ring and *after* the access-rights
gate, never before. A producer waiting for its next event must hold nothing, or
a connection with more subscriptions than `MONITOR_CREDIT` would exhaust the
pool with an empty queue and nothing left to drain to release it.

With no credit the producer parks without dequeuing, so the backlog stays in
the `EvQue` ring and coalesces there — the same place, and for the same reason,
as in C and in `blocking.rs`.

### Re-measurement

Same script, same 100 Hz putter, same flood, same 60 samples over 591 s
([log](ca-stuck-reader/hosted-growth-600-credit.log)):

| | before | after |
|---|---|---|
| `VmRSS` drift | +648 → +6,168 kB | +120 → **+120 kB** |
| slope | 9.340 kB/s (32.8 MB/h) | **0.000 kB/s** |
| server `Send-Q` | 1,357,200, constant | 1,357,152, constant |

The `Send-Q` column is what makes the second run mean anything: it is constant
in both, so the drain is parked for the whole window in both, and the condition
being measured did not change — only the growth did. Monitor throughput for a
client that *does* read is unchanged: the pre-flood phase measured 97.8
events/s against the 100 Hz putter, versus 98.2 before the fix.
