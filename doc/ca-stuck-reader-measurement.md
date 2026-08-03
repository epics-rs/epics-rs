# What bounds a CA client that stops reading, on RTEMS

Measured 2026-08-03 against the live `caioc-deploy.exe` guest on the
RTEMS/QEMU rig (`xilinx-zynq-a9`, 256 MB, hostfwd `tcp:127.0.0.1:5064`).

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
raw CA connections to the guest and never touches the IOC's data. Both runs'
raw output is beside it
([run 1](ca-stuck-reader/stuck-reader-1800.log),
[run 2](ca-stuck-reader/stuck-monitor-1200.log)).

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

## The hosted driver has no stall guard either — and says it does

Found while checking that the 15 s ENOBUFS backoff added in
`server::send` could not be cut short by a send timeout. It cannot, and the
reason is the finding.

`tcp.rs:1484` sets `SO_SNDTIMEO`, and its own comment correctly notes the
kernel does not apply it to a non-blocking tokio socket, so "a stuck client
where the kernel send buffer fills would still leave `poll_write` Pending
forever." It then names the replacement:

> The actual stall guard is the `tokio::time::timeout` wrapping
> `dispatch_message` in `handle_client`'s read loop.

That guard cannot fire on a socket stall. `dispatch_message` takes
`writer: &Outbox` and never touches `sock` — the batch-flush refactor moved
every socket write out to `drain_and_flush` at the bottom of the loop, which is
**not** wrapped in a timeout. The comment at the `timeout` call site
(`tcp.rs:2128`) still states the old purpose, "so a stuck-reader client (kernel
send buffer full → `write_all` Pending forever) can be detected", and two
paragraphs below acknowledges the refactor that removed it.

The second half is the outbox. `server/outbox.rs:73` is
`mpsc::unbounded_channel()`, justified at `:68`:

> Unbounded because the sole draining owner pulls the queue empty after [...]

which stops being true exactly when the drain parks in `write`. Monitor and
put-notify tasks push independently of the read loop, so while the drain is
parked they queue without limit. Pipelined requests do *not* do this — once the
drain parks, the loop stops dispatching, so no new replies are produced. It
takes an asynchronous producer, i.e. a subscription. That is what run 2
attempted.

Both drivers share the exposure: `blocking.rs::drain_outbox_locked` parks under
the send lock while the event worker keeps pushing into the same unbounded
queue.

### Run 2 tried to measure this and failed to create the condition

[`ca-stuck-reader/stuck_monitor.py`](ca-stuck-reader/stuck_monitor.py).
A subscribed to `RTEMS:MEM_FREE` (measured producer rate 1.10 events/s) with a
2 KB receive buffer and read nothing for 1200 s, while B sampled `MEM_FREE`
every 30 s. Guest free memory took exactly two values across the whole run:
−10,664 B for the first 13 samples, then −10,880 B for the remaining 27. One
216-byte step, then flat for 810 s.

**That is not evidence the outbox is bounded.** 1.10 events/s × 1200 s ≈ 1,320
frames of roughly 50 B ≈ 66 KB. If the drain had been parked, that is what
would have accumulated. 216 B did. So the frames left the server, which means
the write path never parked — QEMU's SLIRP hostfwd and the host socket buffers
sit between A and the guest and absorb 66 KB without complaint, so A's small
`SO_RCVBUF` never produced back-pressure at the guest.

The growth question is therefore **open**: not confirmed, not refuted. Closing
it needs a producer big or fast enough to exceed the intermediate buffering —
a waveform monitor rather than a 1 Hz scalar — plus a direct check that the
drain is parked rather than an inference from memory. This guest serves only
scalar `RTEMS:*` stats PVs, so it cannot supply that; the experiment needs a
purpose-built image.

The same confound bounds what run 1 proved. A was demonstrably never dropped
and B was demonstrably never delayed — both end-to-end observations, and both
hold. Whether the server's `write` was parked during those 30 minutes was not
independently verified.

**Not fixed here.** Bounding it means choosing a policy — drop oldest, drop
newest, disconnect at a depth, or block the producer as C does with its fixed
`send.buf` — and that is a behavioural decision, not a defect with one obvious
repair. Recorded for the owner to decide.

**Disclosure about the ENOBUFS change.** The dominant stuck-reader case (zero
window, `poll_write` Pending) was already unbounded and unguarded before that
change; ENOBUFS previously returned `Err` and disconnected the client. So the
fix adds a second, rarer way to park, on a path that was already open. It does
not create the exposure, and it does lengthen it by up to 15 s per ENOBUFS.

## Not transferred to VxWorks

This is an rtems-libbsd result. The VxWorks 7 SDK on the rig ships headers
only — no TCP stack source — so the same reasoning **has not been checked**
there and no claim is made about it. An equivalent on-target run against a
VxWorks RTP image is the way to close that half.
