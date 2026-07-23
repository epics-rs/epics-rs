# PVA phase 6 item 5 — reader / operation / writer split: settled design

**Status:** COMPLETE as built (2026-07-23). Stages 1–5 all landed:
`ad268398` (stage 1), `93590517` (stage 2), `44681c76` (stage 3, as built
§10), `ab97461f` (stage 4, as built §11), stage 5 closed by the executable
gate `scripts/rtems-check.sh` (as built §12). The post-merge owed rename
landed as `04cdf6fa`. §§0–9 are preserved as written — design-time text
whose line numbers reference the pva-cs-ring worktree; §§10–12 record
where the landed code deviated.
**Bases read:**
- PVA (items 4+6 applied): `/home/stevek/work/epics-rs/.caucus/worktrees/pva-cs-ring`, branch `phase6/pva-channelsource-ring` @ `e278088c`.
- CA reference + new base runtime: `/home/stevek/work/epics-rs/.caucus/worktrees/manual-ca-sans-io` @ `d704087d`.
- Plan: `doc/rtems-runtime-portability-design.md` §7, §9 phase 6 (main @ `02ec5082`).

Unqualified `tcp.rs` below means `crates/epics-pva-rs/src/server_native/tcp.rs`
in the **pva-cs-ring** worktree. CA-side and base-runtime citations are
explicitly prefixed.

---

## 0. Headline: the plan's item-5 shape is wrong in one load-bearing way

The plan row reads:

> | 5 | **reader/operation/writer split; frame channel; box 10 op futures into `select_all`** | **2–3 wk** |

The last third of that — *box 10 op futures into `select_all`* — was the answer
to a defect that **no longer exists**. `d704087d` rewrote `future_exec` from
worker-per-future to a cooperative executor:

> "a bounded worker set multiplexes an unbounded number of mostly-idle tails"
> — CA worktree `crates/epics-base-rs/src/runtime/background/future_exec.rs:19-20`

> "That model had a structural defect: N concurrent long-lived tails exhausted
> the band's N workers … Memory note `rtems-exec-worker-per-future-fragility`;
> it mattered more once PVA started sharing this backend."
> — same file, `:27-31`

`select_all` was PVA's workaround for that ceiling. With the ceiling gone, the
10 per-operation futures should stay **spawned through the `runtime::task`
seam**, not be hand-erased into a multiplexer. §2 below proves they qualify.

Second correction, in the other direction: §7 sizes the work as "wiring plus
the RTEMS dependency work". It does not size **(a)** the blocking driver as an
*additive second driver* (the only shape that keeps hosted behaviour
identical), nor **(b)** the connection-shutdown path, which on RTEMS cannot be
`JoinSet::abort` because a connection is a thread. Both are real. Net: §7 below
revises **2–3 wk → 3–4.5 wk**, with a much lower risk profile than the boxing
plan carried.

---

## 1. Question 1 — the seam where the socket await leaves the operation loop

### 1.1 What the operation loop actually is today

`handle_connection_io` (`tcp.rs:3243`) sets up per-connection state and then
runs one async block, `tcp.rs:3481-4140`, whose `select!` (`tcp.rs:3502`) has
**six** arms after item 6:

| arm | line | awaits |
|---|---|---|
| `cc_rx.recv()` | `tcp.rs:3503` | `tokio::sync::mpsc` |
| `mon_fin_rx.recv()` | `tcp.rs:3622` | `tokio::sync::mpsc` (unbounded) |
| `exec_fin_rx.recv()` | `tcp.rs:3642` | `tokio::sync::mpsc` (unbounded) |
| `inv_rx.recv(), if !inv_closed` | `tcp.rs:3653` | `tokio::sync::broadcast` |
| `hb_tick.tick(), if !hb_stopped` | `tcp.rs:3691` | `tokio::time::Interval` (`tcp.rs:3323`) |
| `read_frame(&mut reader, …)` | `tcp.rs:3714` | **the socket** + `tokio::time::timeout` (`tcp.rs:5509`) |

Five of six arms are already runtime-agnostic. **Exactly one arm touches the
socket.** That is the whole seam.

### 1.2 The seam: replace the byte *source*, not the frame *pipeline*

§7 says the reader thread should do "blocking read → frame parse → hand the
frame to the operation thread". I recommend cutting one level lower: **the
reader thread hands bytes, not frames.**

`SrvRead` (`tcp.rs:3121`) is already `Box<dyn tokio::io::AsyncRead + Unpin +
Send>` — a *type-erased* byte source, chosen so plain TCP and TLS share one
handler (`tcp.rs:3119-3120`, constructed at `tcp.rs:2665-2667` and
`tcp.rs:2697-2699`). So the blocking backend does not need a new type at all:
it needs a **new implementor**.

```
// server_native/blocking.rs (new file)
struct ChannelReader { rx: mpsc::Receiver<Vec<u8>>, cur: Vec<u8>, pos: usize }
impl tokio::io::AsyncRead for ChannelReader {
    fn poll_read(..) -> Poll<io::Result<()>>   // drains `cur`, else self.rx.poll_recv(cx)
}
```

fed by a blocking reader thread:

```
loop {
    let n = stream.read(&mut chunk)?;               // std::net::TcpStream, SO_RCVTIMEO
    if n == 0 { break }                            // EOF
    if block_on_sync(tx.send(chunk[..n].to_vec())).is_err() { break }
}
```

**What type replaces `SrvRead`: nothing.** `SrvRead` *is* the seam and it
already exists. The blocking driver passes `Box::new(ChannelReader)` where the
hosted driver passes `Box::new(tokio_reader_half)`.

### 1.3 Why bytes and not frames — the TypeCache invariant

The user's constraint:

> `tcp.rs:3384-3395` … the read loop is the single owner of inbound type-cache
> state … parsing may move, **type-cache resolution must not**.

The code that invariant rests on — **now at `tcp.rs:3418-3428`**; the cited
`:3384-3395` is its address on the pre-item-6 base, the text is unchanged:

> "pvxs keeps one connection-scoped `rxRegistry` (conn.h:23) shared by every
> inbound decode; a client may define a descriptor with `0xFD <slot> <desc>` in
> one frame … and reference it with `0xFE <slot>` in any later frame on the
> same connection. The read loop dispatches every frame synchronously in wire
> order, so a define is always folded into this cache before a later reference
> resolves against it"

Handing **bytes** across the thread boundary means `rx_type_cache`
(`tcp.rs:3428`), `encode_type_cache` (`tcp.rs:3417`), `seg_buf`/`seg_cmd`/
`seg_order`/`expect_seg` (`tcp.rs:3439-3448`), `channels` (`tcp.rs:3380`) and
the frame parse (`try_parse_frame_role`, `tcp.rs:5485`) **all stay exactly
where they are**. Nothing about frame ownership changes. The invariant holds
*by construction* rather than by a new protocol between two threads — which is
the difference between a structural fix and a runtime-checked one.

Cost of the extra hop: one `Vec` alloc + one memcpy per ≤4 KiB chunk
(`read_frame` already copies `chunk` → `rx_buf`, `tcp.rs:5508,5517`). On an
RTEMS IOC that is noise, and it buys a code path that is *identical* to hosted.

### 1.4 Channel type and backpressure

- **Type:** `tokio::sync::mpsc::channel::<Vec<u8>>(N)` — bounded. Same family
  as the writer channel (`tcp.rs:3275`).
- **Depth:** `N = 1`. Today the read is strictly demand-driven — `read_frame`
  issues one `read` per poll and the loop dispatches a frame fully before
  reading again (`tcp.rs:5507-5518`). `N = 1` reproduces that with at most one
  chunk of read-ahead, which the kernel receive buffer already provides anyway.
  Larger `N` would let a fast client queue chunks while a slow source blocks
  the dispatcher — a behaviour change, not an optimisation.
- **Reader-side send:** `block_on_sync(tx.send(buf))`, the house primitive
  (CA worktree `crates/epics-ca-rs/src/server/blocking.rs:501, :657, :753,
  :864`). On a plain thread with no runtime entered it selects `park_on`
  (CA worktree `crates/epics-base-rs/src/runtime/task.rs:112-121`). Do **not**
  use `Sender::blocking_send` — it panics inside a runtime context and would
  make the same file unusable from a hosted `spawn_blocking` thread.

### 1.5 Cancel-safety is preserved, and here is why

`read_frame` is used directly as a `select!` arm (`tcp.rs:3714`) and is
cancel-safe only because `rx_buf` is *external* to it (`tcp.rs:5475`, the `rx_buf: &mut Vec<u8>` parameter): bytes
already accumulated survive a lost race, and the in-flight
`reader.read(&mut chunk)` (`tcp.rs:5509`) consumes nothing unless it completes.

`ChannelReader::poll_read` has the same property: `rx.poll_recv(cx)` consumes a
chunk only when it returns `Ready`, and a partially-consumed chunk lives in
`self.cur`/`self.pos` across cancellations. **A test must prove this** — see
stage 3's test list, because "the adapter loses a chunk on a lost select race"
is the one failure mode that would be silent and intermittent.

### 1.6 The read timeout

`read_frame` bounds each socket read with `op_timeout` (`tcp.rs:5509`). On the
blocking reader that becomes `TcpStream::set_read_timeout(Some(op_timeout))` =
**SO_RCVTIMEO**, portable to RTEMS — the CA driver's exact move and its exact
justification (CA worktree `blocking.rs:617-621`: "`set_read_timeout` sets
SO_RCVTIMEO on Unix (incl. RTEMS) and is portable"), with `is_read_timeout`
(`blocking.rs:560`) classifying `WouldBlock|TimedOut` as the timeout firing.

**Sharp edge — do not miss this.** `op_timeout`'s default is ~64,000 s
(`tcp.rs:3487-3488` comment). CA can lean on its read timeout to unblock a
stuck reader because CA's is `inactivity_timeout()` ≈ 45 s. PVA's cannot: an
SO_RCVTIMEO of 64,000 s is effectively infinite, so **SO_RCVTIMEO must not be
the shutdown mechanism**. See §4.

---

## 2. Question 2 — the 10 per-operation futures

### 2.1 Inventory (production sites, `tcp.rs`)

| # | line | op | terminal signal | RAII guard |
|---|---|---|---|---|
| 1 | `tcp.rs:1947` | MONITOR subscriber | `mon_fin_tx` | `MonitorFinishGuard` (`tcp.rs:863`) |
| 2 | `tcp.rs:4660` | PUT_GET EXEC | `exec_fin_tx` | `ExecFinishGuard` (`tcp.rs:967`) |
| 3 | `tcp.rs:5006` | PROCESS EXEC | `exec_fin_tx` | `ExecFinishGuard` |
| 4 | `tcp.rs:5375` | ARRAY sub-op EXEC | `exec_fin_tx` | `ExecFinishGuard` |
| 5 | `tcp.rs:5664` | CREATE_CHANNEL batch resolver | `cc_tx` | — (drops `pending_channel_spawns` via the completion) |
| 6 | `tcp.rs:6987` | GET EXEC | `exec_fin_tx` | `ExecFinishGuard` |
| 7 | `tcp.rs:7137` | PUT readback GET EXEC | `exec_fin_tx` | `ExecFinishGuard` |
| 8 | `tcp.rs:7286` | PUT EXEC | `exec_fin_tx` | `ExecFinishGuard` |
| 9 | `tcp.rs:7653` | RPC EXEC | `exec_fin_tx` | `ExecFinishGuard` |
| 10 | `tcp.rs:7881` | GET_FIELD introspection | `exec_fin_tx` | `ExecFinishGuard` |

(The 11th production spawn, `tcp.rs:3278`, is the writer — §3.)

### 2.2 What each needs from the connection context

All ten capture **by value/clone only**; none borrows loop-owned state. Sampled
capture sets, all identical in shape (`tcp.rs:4641-4659`, `:4987-5005`,
`:6961-6986`, `:7111-7136`, `:7261-7285`, `:7627-7652`, `:7855-7880`,
`:5646-5663`):

- `src: DynSource` — clone of the channel's **bound owner**, not the registry
  (`tcp.rs:4641`, `:4987`, `:7261`).
- `chan_tx: ChannelTx` (`tcp.rs:3152`, `send` at `:3165`) — the writer handle *plus* the channel's
  `statTx` accounting. This is the only way an op emits a per-channel frame,
  by construction (`tcp.rs:3136-3151`).
- `order: ByteOrder` — a **copy**. The monitor subscriber (site 1) is the
  exception: it outlives a mid-stream SET_BYTE_ORDER, so it holds
  `out_order: Arc<AtomicBool>` and reads it per frame via `order_now()`
  (`tcp.rs:3336-3342`).
- credential *fields* cloned individually at dispatch —
  `cred.{account,method,host,authority,roles}` (`tcp.rs:6961-6964`). This
  snapshot-at-dispatch is deliberate: a mid-flight re-auth must not change the
  identity an in-flight op runs under (`tcp.rs:5655-5661`).
- `peer: SocketAddr` (copy), `pv_name: String`, `sid`/`ioid`/`op_id`,
  `init_pv_request: Option<..>` (clone), `intro: Option<FieldDesc>` (clone).
- the terminal-signal sender: `exec_fin_tx` / `mon_fin_tx` / `cc_tx`.

**None of them touches `channels`, `rx_type_cache`, `encode_type_cache`,
`seg_buf`, or the loop's `order` local.** That is not an accident — the
owner pattern exists precisely to forbid it (`tcp.rs:938-950`: "The spawned
task owns no handle to `channels`, so it cannot return its own op to `Idle`;
it reports its identity here and the read-loop owner … applies the
transition"). Every capture set is already `'static + Send`.

### 2.3 Their await set is already runtime-agnostic — verified by absence

A production-only sweep (everything above the first column-0 `#[cfg(test)]`) of
`crates/epics-pva-rs/src` for `tokio::{time,net,spawn,task::spawn}`:

- `server_native/source.rs`, `server_native/shared_pv.rs`,
  `server_native/composite.rs`, `server/native_source.rs` — **zero hits**.
  That is the entire call graph the op futures descend into.
- `server_native/tcp.rs` production `tokio::time`: lines 22 (import), 2584,
  2635, 2724, 3280, 3323, 5509 — **none inside an op future**. 2584/2635/2724
  are the accept path (item 7); 3280 is the writer (§3); 3323 is the heartbeat
  arm; 5509 is `read_frame` (§1.6).
- `server_native/tcp.rs` production `tokio::net`: line 20 only (`TcpListener`,
  accept path).

So all ten satisfy `future_exec`'s stated precondition verbatim — "All
primitives the future awaits must still be runtime-agnostic (`tokio::sync`
locks/channels/notifies, `timer_sleep`)" (CA worktree
`future_exec.rs:21-23`). They await `tokio::sync::{mpsc, RwLock, watch,
Notify, oneshot}` and nothing else.

The monitor subscriber (site 1) is the one worth calling out: its `select!`
(`tcp.rs:2282-2298`) awaits `MonitorStream::recv` (item 4 — `Notify`/mpsc),
`exec_rx.changed()` (watch), `wait_credit_refill` (`tcp.rs:1420-1427`, a
`Notified` or `pending()`), and `ChannelTx::send` on a bounded mpsc. **No
timer.** Under the cooperative executor a full writer channel suspends it and
*releases the band worker* — which is exactly the fragility `d704087d`
removed, and what makes M monitors per connection safe on RTEMS.

### 2.4 The RAII guards survive — three drop paths, all verified

Under the executor, a task's future is dropped (running destructors, hence
`Drop for ExecFinishGuard` at `tcp.rs:983` and `Drop for MonitorFinishGuard` at
`tcp.rs:868`) on **every** terminal path:

1. **abort** — `future_exec.rs:343-348`: "Dropping `fut` here runs its
   destructors, exactly as a cancelled tokio task's does."
2. **ring full / band shutdown** — `Entry::Drop`, `future_exec.rs:471-479`:
   takes and drops the future, then `finalize(cancelled)`.
3. **unreachable task** — `Task::Drop`, `future_exec.rs:442-448`.

And `future_exec.rs:73-79` states the closure explicitly: "A `JoinFuture`
therefore never strands." Both guards send on an **unbounded** mpsc
(`tcp.rs:869-874`, `:984-988`) so the sync `Drop` never blocks a band worker.

`AbortOnDrop` (`tcp.rs:783-789`) wraps `tokio::task::AbortHandle`; it becomes
`runtime::task::TaskAbortHandle` (CA worktree `task.rs:136-146`), whose
`abort()`/`is_finished()` are the exact subset in use.

### 2.5 Answer

**Item 5 does not box the op futures into `select_all`.** It swaps 11
`tokio::spawn` calls to `epics_base_rs::runtime::task::spawn` and one handle
type. Rationale, beyond §0: erasing ten heterogeneous futures into
`Pin<Box<dyn Future>>` and hand-rolling their completion routing would
*replace* a working, id-guarded owner lifecycle
(`apply_monitor_finish` `tcp.rs:884`, `apply_exec_finish` — with the ABA guards
at `tcp.rs:849-857` and `tcp.rs:938-950`) with a new hand-written one. That is
a large regression surface for zero benefit now that the worker ceiling is
gone. `run_event_task` (CA worktree `tcp.rs:5645`) stays CA's shape because CA
built it *before* the executor and shares it with CA's hosted driver
(CA worktree `tcp.rs:5297`); it is not a pattern PVA should import.

---

## 3. Question 3 — the writer side

### 3.1 Today

`tcp.rs:3275` bounded `mpsc::channel::<Vec<u8>>(config.write_queue_depth)`
(default 1024, `server_native/runtime.rs:355`); `tcp.rs:3278` a spawned task
drains it; `tcp.rs:3280` wraps each `write_all` in
`tokio::time::timeout(send_tmo, …)` (default 5 s,
`server_native/runtime.rs:361`); `tcp.rs:3307` an `AbortOnDrop` guard kills it
when the loop returns.

The timeout's stated purpose (`tcp.rs:3262-3273`): a peer that stops reading
fills the kernel send buffer, `write_all` never completes, and the writer
"would … back-pressure both the heartbeat and the read-side dispatcher (since
both push into the same mpsc)".

### 3.2 Why the writer must be a *thread*, not a tail

This is the one place PVA must diverge from CA, and §7's trio is right for a
reason §7 does not give.

CA has **no third writer thread**: writes are serialised by an
`Arc<Mutex<TcpStream>>` send lock shared between the dispatch thread and the
`CAS-event-blocking` thread (CA worktree `blocking.rs:624-628`, `:652-657`,
`write_frame_locked` `:568`), mirroring C `client->lock` / `SEND_LOCK`
(`server.h:221`). That works because **both CA writers are threads** — a
blocking `write` parks a thread that owns nothing else.

PVA's producers are the operation thread *and* M monitor subscriber tails on a
shared band. If a tail wrote the socket directly, a blocking `write_all` would
hold a band worker for the duration — breaking the cooperative model
(`future_exec.rs:19-21`). So PVA keeps the mpsc and adds a dedicated writer
**thread**. Three threads per connection, as §7 says.

### 3.3 SO_SNDTIMEO is a *weaker* bound — close the gap, don't accept it

`std::net::TcpStream::set_write_timeout` → SO_SNDTIMEO on Unix (symmetric with
CA's `set_read_timeout`, `blocking.rs:620`). But the semantics differ from what
it replaces:

- `tokio::time::timeout(send_tmo, write_all(&frame))` bounds the **whole
  frame**.
- SO_SNDTIMEO bounds **each `write` syscall**.

A client that accepts one byte every `send_tmo - ε` never trips SO_SNDTIMEO and
holds the writer thread indefinitely — reintroducing exactly the stuck-client
hazard `tcp.rs:3262-3273` was written to prevent, on a resource (an OS thread)
that is scarcer on RTEMS than a tokio task is on the host.

**Recommendation: a deadline loop, not `write_all`.**

```
let deadline = Instant::now() + send_tmo;
let mut off = 0;
while off < frame.len() {
    match stream.write(&frame[off..]) {
        Ok(0) => return Err(..),
        Ok(n) => off += n,
        Err(e) if e.kind() == Interrupted => continue,
        Err(e) if is_write_timeout(e.kind()) => {}      // SO_SNDTIMEO tick
        Err(e) => return Err(e),
    }
    if Instant::now() >= deadline { return Err(timed_out) }
}
```

with SO_SNDTIMEO set to a fraction of `send_tmo` so the loop gets to re-check
its deadline. ~15 lines, and it preserves the stated security property exactly
rather than silently weakening it. `is_write_timeout` reuses CA's
`WouldBlock|TimedOut` classification (`blocking.rs:560-566`).

Partial-write desync on timeout is harmless: both the current code
(`tcp.rs:3287-3295`) and this one react by ending the writer and tearing the
connection down. Nothing is ever written to that socket again.

### 3.4 Structure

`writer_raw: SrvWrite` (`tcp.rs:3120`) is, like `SrvRead`, already the seam.
The blocking driver passes a `ChannelWriter` whose `poll_write` pushes into a
bounded mpsc drained by the writer thread; the existing writer *task*
(`tcp.rs:3278`, becoming `runtime::task::spawn`) then never blocks — its
`write_all` is a channel send that suspends and releases the worker.

That keeps `handle_connection_io` byte-identical across both drivers. The
alternative — cfg-ing the writer task out and having the blocking driver own
the drain — forks the frame-emission path, which is the thing §1.3's reasoning
says not to do.

---

## 4. Question 4 — shutdown ordering across the three threads

### 4.1 Owner

The **operation thread**. It runs `handle_connection_io` and its return value
*is* the connection's result (`tcp.rs:4140`). The reader and writer threads are
its children and neither may decide the connection is over — they can only
report.

### 4.2 The three termination triggers

**(a) Reader ends** — EOF (`read` → 0), read error, SO_RCVTIMEO past
`op_timeout`, or `max_message_size` violation. The reader drops its `Sender`;
`ChannelReader::poll_read` then returns `Ok(())` with 0 bytes filled;
`read_frame` sees `n == 0` and returns `Err(Protocol("client closed"))`
(`tcp.rs:5514-5516`) — **the existing EOF path, unchanged**. `?` at
`tcp.rs:3715` exits the loop. This is the common case and it needs no new
machinery at all.

**(b) Writer ends** — write error or the §3.3 deadline. Today this is detected
by the loop-top guard `if tx.is_closed() { return Ok(()) }` (`tcp.rs:3494`),
whose comment explains it exists so teardown happens "within ms instead of
~30-45 s" (`tcp.rs:3483-3493`).

> **Gap, present today and worse on RTEMS.** That guard is at the *loop top*.
> A loop parked in `select!` with no ready arm does not reach it. The only
> thing that guarantees it is reached is the heartbeat arm — i.e. up to **15 s**
> (`tcp.rs:3323`), during which two threads and a socket are held.
>
> **Fix (structural, and it needs sign-off because it changes hosted timing):**
> the writer holds a `oneshot::Sender` dropped on exit; the operation loop
> selects on the matching `oneshot::Receiver`, which resolves `Err(RecvError)`
> the moment the writer dies. Seventh arm, ~6 lines, removes the 15 s window on
> **both** builds. It makes hosted teardown *faster*, which is a behaviour
> change and therefore a sign-off item, not something to slip in.

**(c) Server shutdown.** Today the accept loop holds every connection in a
`JoinSet` (`tcp.rs:2502-2506`) and dropping the accept future aborts them all.
**On RTEMS a connection is a thread and threads cannot be aborted.** This is
the piece §7 does not mention and it is the real risk in item 5.

The mechanism must be CA's: a flag plus a syscall that unblocks the blocked
thread. `BlockingCaServer::shutdown` (CA worktree `blocking.rs:228`) sets an
`AtomicBool` and then dials its own listening socket to wake `accept()`. The
connection-level equivalent is `TcpStream::shutdown(Shutdown::Both)` on the
connection's fd from a server-side registry, which makes the reader's blocking
`read` return 0 at once. **SO_RCVTIMEO cannot substitute** — §1.6: `op_timeout`
defaults to ~64,000 s.

So the blocking driver needs a per-connection registry of
`Arc<std::net::TcpStream>` (or a dup'd fd) that server shutdown walks. That is
its own stage (§6, stage 4).

### 4.3 The exit sequence, in order, on the operation thread

1. **Channels first, unchanged.** `for (_sid, ch) in channels.drain() {
   close_channel(ch, peer) }` (`tcp.rs:4137-4139`). This fires each source's
   `onClose` and drops each `OpState`, whose `data_task_abort:
   Option<Arc<AbortOnDrop>>` (`tcp.rs:1245`) aborts the op tails. Under the
   executor, abort drops each future → `ExecFinishGuard`/`MonitorFinishGuard`
   fire (§2.4). Their sends land on a receiver the loop no longer reads, which
   is fine — the mpsc is unbounded and dropped next.
2. **Writer down, then join.** Drop every `Sender` clone of the writer channel
   (the loop's `tx`, every `ChannelTx`) → the writer task's `rx.recv()` returns
   `None` → it drains what is queued and exits → the writer *thread*'s channel
   closes → **`join()` it**.
3. **Reader down, then join.** `stream.shutdown(Shutdown::Both)` → the reader's
   blocking `read` returns 0 → the reader thread exits → **`join()` it**.
4. Return `conn_result`.

Both joins are mandatory. CA does exactly this and says why: "Dropping the
control sender ends `run_event_task`; join so the second thread never leaks
(C `db_close_events` + `event_task` exit)" — CA worktree `blocking.rs:963-964`.
Without the joins a thread outlives the connection holding a socket fd, which
on RTEMS is a hard resource leak, not a GC problem.

Ordering rationale: writer before reader, because step 1's teardown can still
emit frames (MONITOR FINISH, DESTROY_CHANNEL) and those should reach the wire
before the socket is torn down. Shutting the socket first would silently drop
them.

### 4.4 Error propagation summary

| origin | vehicle | operation loop sees |
|---|---|---|
| socket EOF / read error / RCVTIMEO | reader drops `Sender` | `read_frame` → `Err(Protocol("client closed"))` (`tcp.rs:5514`) → `?` (`tcp.rs:3715`) |
| oversize frame | reader (has `max_msg_size`) or loop (`tcp.rs:5491-5506`) | `Err(Protocol)` → `?` |
| write error / send deadline | writer drops `oneshot::Sender` | new 7th arm (§4.2b); fallback = `tx.is_closed()` at `tcp.rs:3494` |
| decode / protocol violation | already in-loop | `?` (`tcp.rs:3764-3770`, `:3786-3791`) |
| server shutdown | registry `shutdown(Both)` | same as EOF |

---

## 5. Question 5 — what items 4+6 already removed from item 5's scope

### 5.1 Per-monitor-op

Item 4 (`72239333`) made `ChannelSource` return `MonitorStream<T>` — a **pull**
model — and deleted all six bridge tasks. `tokio::spawn` in
`server_native/shared_pv.rs`, `server/native_source.rs` and
`server_native/source.rs` is now **zero** (verified in §2.3's sweep). Item 5
therefore does not have to:

- migrate 1–2 copy tasks per monitor to the blocking backend;
- decide how a `MonitorRing` is drained across a thread boundary — it is not;
  the consumer pulls, and `recv`/`try_recv` await only `Notify`/mpsc;
- preserve the empty-mask filter or the connect-time seed across a task split —
  `UpstreamMonitor` applies both on pull inside the consumer's own future.

Item 6 (`e278088c`) deleted `spawn_monitor_gate_driver`, which was spawned per
**gated** monitor op (only a QSRV db/group monitor supplies a gate — commit
message, `e278088c`). Item 5 does not have to migrate a cross-task watch
translation; the gate is applied at the subscriber's loop top (`tcp.rs:2256`).

Residue per monitor op: **exactly one tail** (`tcp.rs:1947`).

### 5.2 Per-connection

Item 6 (`f0ca0909`) folded the heartbeat task into the loop's `select!`
(`tcp.rs:3691`), and with it two structural collapses: `last_rx` went
`Arc<AtomicU64>` → a plain local (`tcp.rs:3313`), and the heartbeat's
`AbortOnDrop` guard disappeared (`tcp.rs:3304-3307`).

For item 5 that means the operation thread has **one timer to re-home**
(`tcp.rs:3323`, `tokio::time::interval` → `runtime::task::interval`, which
exists on the CA branch: `task.rs:284-301`) instead of a whole task to migrate,
and one fewer abort-handle lifetime to reason about during shutdown.

### 5.3 Task/thread inventory delta

Per connection with M monitors:

| | tasks/threads |
|---|---|
| before items 4+6 (hosted) | read loop + writer + heartbeat + M × (subscriber + gate driver? + 1–2 bridges) |
| after items 4+6 (hosted) | read loop + writer + M × subscriber = **2 + M** |
| item 5 (RTEMS) | operation thread + reader thread + writer thread + M tails on the shared band = **3 threads + M tails** |

The M went from tasks-that-cost-a-worker to tails-that-cost-a-poll — half from
item 4 deleting them, half from `d704087d` making the survivors cheap.

---

## 6. What is *not* in item 5

Named so the boundary is explicit, not so they are forgotten:

- **Accept loop, UDP responder, beacons** (`tcp.rs:2502` accept, `:2584`,
  `:2635`, `:2724`; `server_native/udp.rs`; `server_native/runtime.rs:850-1007`)
  — item 7.
- **The remaining 6 of 9 timer sites and the client-side timers** — item 8.
- **The full 87-site `abort()` → `TaskHandle` sweep** — item 9. Item 5 touches
  only the 11 connection-scope spawns and `AbortOnDrop` (`tcp.rs:783`).
- **TLS** (`tcp.rs:2635-2699`) — gated off RTEMS by item 1 (`1d5476df`). The
  blocking driver is plain-TCP only; `SrvRead`/`SrvWrite` erasure means that is
  a construction-site difference, not a code fork.
- **The PVA client's same-shaped connection trio**
  (`client_native/server_conn.rs:350, :391, :637`) — out of scope by the
  server-only decision (plan §9 phase 6).

---

## 7. Staged commit plan

Prerequisite: **CA branch merged to main**, which is what brings
`runtime::task::{spawn, TaskHandle, TaskAbortHandle, interval, background_init}`,
`future_exec`, `timer_sleep`, and the `blocking.rs` reference into `epics-base-rs`.
Verified absent on the current PVA base: `crates/epics-base-rs/src/runtime/background/`
does not exist, `runtime::task::spawn` returns a bare `tokio::task::JoinHandle`
(`task.rs:83`), and there is no `interval` and no `TaskHandle` alias.

Every stage is independently workspace-green
(`cargo nextest run --workspace` + `cargo clippy --workspace --all-targets
-- -D warnings`; baseline at branch head is **9754**).

| # | stage | green? | desktop-neutral? | size |
|---|---|---|---|---|
| 1 | `runtime::task::timeout(dur, fut)` in `epics-base-rs` — the one seam gap. `tokio_backend` → `tokio::time::timeout`; `exec_backend` → `select(timer_sleep::sleep(dur), fut)`. Boundary tests: fires, does-not-fire, already-elapsed, cancel-drop. | ✅ | ✅ | 1–2 d |
| 2 | PVA server-native seam swap: 11 `tokio::spawn` → `runtime::task::spawn`; `AbortOnDrop(TaskAbortHandle)` (`tcp.rs:783`); 3 timer sites (`:3280`, `:3323`, `:5509`) onto the seam. Aliases on hosted ⇒ zero behaviour change. | ✅ | ✅ | 3–4 d |
| 3 | `server_native/blocking.rs`: `ChannelReader`/`ChannelWriter` adapters + reader/writer threads + `block_on_sync(handle_connection_io(..))`. Host-compiled and host-tested against a real loopback client, mirroring CA (`blocking.rs` is not cfg'd out), **including** the static `blocking_driver_has_no_async_runtime_symbols` guard (CA worktree `blocking.rs:984`). Tests: adapter cancel-safety under a lost select race; partial-header and partial-body frames; segmented message across a chunk boundary; a `0xFD` define and a `0xFE` reference in **different** frames (the TypeCache invariant, §1.3); writer deadline loop vs a non-reading client. **DONE** (`44681c76`; as built: §10) | ✅ | ✅ (additive) | 1–1.5 wk |
| 4 | Shutdown: per-connection socket registry + `shutdown(Shutdown::Both)`; the exit sequence and both joins (§4.3); the writer-exit `oneshot` arm. **Sign-off required** — the `oneshot` arm makes hosted teardown immediate instead of ≤15 s. Tests: N connections + server stop ⇒ every thread joined and no fd leak; writer death ⇒ connection retired in ms; client disconnect mid-MONITOR ⇒ `MonitorFinishGuard` fired. **DONE** (`ab97461f`; as built: §11 — the `oneshot` arm was NOT taken, the sign-off item dissolved) | ✅ | ✅ as landed (no hosted timing change) | 3–4 d |
| 5 | RTEMS gate: `cargo +nightly check --target armv7-rtems-eabihf` green for the blocking driver's module set; record the `--extern` set as phase 5 did (`15f1fc6c`). Feeds item 10. **DONE** (superseded in shape by `scripts/rtems-check.sh`; the owed `--extern` record is §12.2) | ✅ | ✅ | 2–3 d |

Sequencing note: stages 1–2 are desktop-neutral and can land **before** the CA
merge lands stage 3's prerequisites… except stage 1 *is* an `epics-base-rs`
change and stage 2 depends on it, so in practice both wait for the merge.
Stage 2 alone is worth landing early once unblocked: it removes item 9's PVA
share and makes stage 3 a pure addition.

---

## 8. Revised size estimate

**3–4.5 engineer-weeks** (plan says 2–3 wk).

Cheaper than planned:
- **The `select_all` boxing is deleted outright** (§2). This was the largest
  single assumed cost and it is not needed after `d704087d`. All ten op futures
  already have `'static + Send` captures and a runtime-agnostic await set —
  verified by absence across the whole server source layer (§2.3) — so they
  need a spawn-call swap, not a restructuring.
- Items 4+6 removed 1–2 bridge tasks per monitor, the gate-driver task, and the
  heartbeat task (§5).
- `SrvRead`/`SrvWrite` already being type-erased means the reader/writer seam
  costs an *implementor*, not a refactor of the connection handler (§1.2, §3.4).

More expensive than planned:
- The blocking driver must be an **additive second driver**, host-compiled and
  host-tested (CA's shape), or "hosted behaviour must not change" cannot be
  proven. §7 sized this as "wiring". It is stage 3, ~1–1.5 wk.
- **Connection shutdown has no analogue in the hosted design** — `JoinSet::abort`
  does not exist for threads, and PVA's 64,000 s `op_timeout` rules out relying
  on SO_RCVTIMEO the way CA relies on its 45 s one (§1.6, §4.2c). That is
  stage 4, ~3–4 d, and it is the highest-risk item now that boxing is gone.
- One genuine seam gap: `runtime::task` has `sleep`/`sleep_until`/`interval` but
  **no `timeout`** (CA worktree `task.rs:238-301`), and PVA needs it at
  `tcp.rs:3280`/`:5509`. Stage 1.

Confidence caveat: the 1–1.5 wk on stage 3 assumes `ChannelReader` cancel-safety
behaves as §1.5 argues. That is the one claim in this document I have reasoned
about but not executed. If it does not hold, the fallback is §7's original
frame-channel split (parse on the reader thread), which is strictly more work
and reopens the TypeCache question — call it +1 wk of downside risk.

---

## 9. Open items needing a decision before stage 3

1. **Writer-exit `oneshot` arm (§4.2b).** Removes a 15 s teardown window on
   both builds; changes hosted timing. Land it, or preserve today's timing and
   accept the window on RTEMS too?
   **RESOLVED in stage 4 (`ab97461f`): neither.** The dichotomy was false —
   waking through the socket closes the window on the blocking driver
   without a seventh arm and without touching hosted timing. See §11.1;
   the sign-off item dissolved instead of being decided.
2. **Frame channel depth (§1.4).** `N = 1` for behavioural identity, or a
   larger depth for read-ahead throughput? I recommend 1 and measuring later.
   **RESOLVED in stage 3 (`44681c76`): depth = 1**, for the reason §1.4 gives —
   the hosted read is demand-driven, so 1 reproduces it and a deeper queue
   would be a behaviour change, not an optimisation.
3. **Writer deadline loop (§3.3)** vs plain SO_SNDTIMEO. I recommend the loop —
   SO_SNDTIMEO alone silently weakens a stated anti-DoS property.
   **RESOLVED in stage 3 (`44681c76`): the deadline loop**, mutation-proved —
   deleting the deadline check and leaving only SO_SNDTIMEO left the
   trickle-client test never returning (killed at 120 s). §3.3's argument,
   executed. The loop lives at `write_frame_deadline`, since `8024b175` in
   `epics-base-rs`'s `runtime::blocking_io` (§10.3).

---

## 10. Stage 3 as built — where reality deviated from §1/§3/§7

Stage 3 landed as `44681c76` (`server_native/blocking.rs`, 1,071 lines +
the `mod.rs` mount) on the integration branch. The load-bearing shape
held exactly as designed: three threads per connection, the threads hand
**bytes not frames** (§1.3 — the `0xFD`-define / `0xFE`-reference
cross-frame test is in the landed test list, end to end), `ChannelReader`
is a new `SrvRead` implementor rather than a new seam, no `cfg` is
threaded through the 21,000-line protocol module, and the hosted driver
is untouched. Both §9 decisions this stage owned were resolved as
recommended (depth = 1; deadline loop — see §9 for the mutation
evidence). Four points did not survive contact unchanged:

### 10.1 Teardown determinism is a WeakSender rule, not just a drop order

§3.4 said the writer-side adapter "pushes into a bounded mpsc" and §4.3
step 2 said "drop every `Sender` clone". As built the rule is stronger
and structural: **the driver holds the only strong `Sender` of the frame
channel; `ChannelWriter` holds a `mpsc::WeakSender`.** That is what makes
dropping the driver end the writer pump deterministically instead of
whenever the runtime reaps an aborted task. Mutation-proved: giving the
adapter a strong `Sender` hangs teardown while every end-to-end test
still passes, which is why the rule has its own test.

### 10.2 The hosted host-runtime boundary is a refusal, not a footnote

§1.4 named `block_on_sync` as the reader-side send primitive and left it
at that. As built, `serve_connection_blocking` on a *hosted* build must
run on a multi-thread runtime worker (the connection future still awaits
tokio-backed seam primitives there), and a current-thread runtime is
**refused with an error rather than deadlocked** — the boundary has its
own test. On RTEMS the exec backend supplies both halves and the same
call runs on a bare thread.

### 10.3 The byte-source primitive did not stay in `epics-pva-rs`

The design put `ChannelReader`/`ChannelWriter` in
`server_native/blocking.rs` (§1.2). They landed there, and then
`8024b175` promoted them — both pump bodies, `write_frame_deadline`, and
both thread-lifecycle guards — into **`epics_base_rs::runtime::blocking_io`**,
because the PVA client's `connect_blocking` and the coming blocking CA
client need the identical primitive and `epics-ca-rs` structurally cannot
depend on `epics-pva-rs` (`doc/calink-rtems-design.md` §3.3 measured
this and named the destination). `blocking.rs` keeps what is
server-side: `ConnRegistry`, `serve_connection_blocking`,
`BlockingPvaServer`. One follow-up the move cost: the file's
RTEMS-EXEC-MODEL-ALLOW census marker went stale (18 → 16) and the
feature-ON gate caught it; corrected in `2c5155c6`.

### 10.4 §1.5's one unexecuted claim was executed, and held

The confidence caveat in §8 — `ChannelReader` cancel-safety "reasoned
about but not executed" — is closed. The landed adapter test covers both
boundaries (mid-chunk and pending) and is mutation-checked: dropping the
parked tail instead of keeping it in `cur`/`pos` hangs the named test
while **every end-to-end test still passes** — exactly the silent,
intermittent failure mode §1.5 predicted, which is why the adapter has
its own test rather than relying on the e2e suite. The +1 wk downside
risk (frame-channel fallback) was not spent.

---

## 11. Stage 4 as built — where reality deviated from §4/§7

Stage 4 landed as `ab97461f` (connection registry + socket-shutdown
teardown, `server_native/blocking.rs` +608 lines). The §4.2c mechanism
held exactly: one way to wake a parked connection thread —
`shutdown(Shutdown::Both)` — and an owner for it. `ConnRegistry` carries
the invariant as MUST/MUST-NOT with both halves holding by construction
(`ConnWake` constructible only by `ConnRegistry::register`;
`serve_connection_blocking` takes `&ConnRegistry` by value, not option;
`ConnRegistration::drop` is the only remover). `stop` is a one-way
latch: a connection registering after it is woken as it registers.
Mutation-checked at the invariant's three boundaries (wake as no-op;
drop skipping deregister; writer dropping its exit wake). Three
deviations:

### 11.1 The writer-exit `oneshot` arm was not taken — §4.2b's fix dissolved

The design proposed a `oneshot` plus a seventh `select!` arm in the
shared connection loop, and flagged it for sign-off because it would
speed *hosted* teardown too. As built, **neither branch of that
trade-off was taken**: a dead writer shuts the socket down, the reader's
blocking `read` returns 0, and the connection unwinds down the existing
EOF path (§4.4's first row). `tcp.rs` and `accept.rs` are untouched, so
the hosted `select!` is byte-identical, hosted teardown timing is
unchanged, and the ≤15 s window §4.2b described is closed on the
blocking driver anyway. This also honours the item-7 design's §6 rule
the `oneshot` variant would have bent: no `cfg`-ed arm inside the
protocol module. The §4.4 error-propagation table's "write error"
row therefore routes through the same vehicle as EOF, not a new arm.

### 11.2 `PvaServer::stop` is deliberately not wired to the registry

The design's §4.2c implied server shutdown walks the registry from the
server object. As built the hosted `PvaServer` does not: `mod.rs` gates
`runtime` and `accept` out of RTEMS while `blocking` is ungated, so a
`PvaServer::stop` arm reaching blocking connections would be dead code
on host and absent on RTEMS. The registry's caller is item 7's blocking
accept loop (`BlockingPvaServer`, landed later as `1c27465c`), which
owns the `ConnRegistry` precisely because `serve_connection_blocking`
cannot be called without one.

### 11.3 `8024b175` narrowed the wake authority after the stage landed

As landed, the writer pump took a registry-issued `ConnWake` (and the
reader guard another). The byte-source promotion (§10.3) separated
plumbing from authority: both pumps now derive their self-retirement
wake from the `Arc<TcpStream>` they already hold, and
`ConnRegistration::wake_handle` is gone with its last caller. The
registry keeps exactly what §4 gave it — the server-WIDE stop — and
nothing else can shut a connection's socket through it.

---

## 12. Stage 5 as built — the one-off check became an executable gate

### 12.1 Superseded in shape: `scripts/rtems-check.sh` is the stage

The stage row asked for a one-off `cargo +nightly check --target
armv7-rtems-eabihf` run over the blocking driver's module set, recorded
the way `15f1fc6c` recorded the CA entry point's. Between design and
close, the gate became an executable census instead of a prose
invocation: `scripts/rtems-check.sh` compiles `epics-pva-rs --lib`
(`server_native::blocking` is ungated in `mod.rs`, so the blocking
driver IS in that module set) with `--locked --no-default-features
-Zbuild-std`, in **both** the portability and the image
(`rtems_boot_linked`) configurations, plus the binary census and the
client-features ratchet. That is strictly stronger than the designed
one-off: it cannot drift, and it already carried the interim records —
stage 3's commit measured exit 0 with the module's dead-code warnings
dropping 116 → 4 (the driver is the RTEMS-side entry point into `tcp`
that the accept-split gate re-point had left missing), stage 4's
measured exit 0 with the count unchanged at 4.

Measured at close (2026-07-23, this tree): `./scripts/rtems-check.sh`
exit 0 — every crate and target binary, both configurations, PVA client
ratchet at 0.

### 12.2 The owed `--extern` record

From the `epics_pva_rs` rustc invocation of `cargo +nightly check
--locked --no-default-features -Zbuild-std=std,panic_abort --target
armv7-rtems-eabihf -p epics-pva-rs --lib -v` (portability
configuration, exit 0):

```
bytes  chrono  clap  dashmap  epics_base_rs  futures_util  hostname
libc  parking_lot  serde_json  thiserror  tokio  tokio_util  tracing
tracing_subscriber
(+ epics_macros_rs as a host-side proc-macro .so; sysroot crates from
-Zbuild-std: std/alloc/core/compiler_builtins/panic_abort/panic_unwind)
```

**No `socket2`, no `if-addrs`, no `getrandom`, no `mio`** — the §8.1.1
lib gating holds at the blocking driver's scope, the same four absences
`15f1fc6c` recorded for `rtems-ca-ioc`. `tokio` is present and stays:
the driver awaits `tokio::sync` primitives through the seam; what the
target excludes is the reactor (`tokio::net`, and mio under it), not
the sync surface. The invocation carries `--cfg exec_backend` — since
the CA merge, transport/dial selection keys on that cfg rather than on
`target_os` directly, which is why the same flag drives the
host-selectable `rtems-exec-model` feature.

### 12.3 Post-merge seam growth the stages absorbed without reshaping

§7's prerequisite list (`spawn`, `TaskHandle`, `TaskAbortHandle`,
`interval`, `background_init`, `future_exec`, `timer_sleep`) arrived
with the CA merge and kept growing: stage 1's `timeout` is no longer a
thin tokio delegation but a dual-backend pair (`task.rs:508`/`:515`),
and the seam now also carries `timeout_at`, a backend-selected
`Instant` alias, and `TaskSet`. None of it changed any stage's shape —
the connection loop's three timer sites still name only the seam.

### 12.4 Item close-out: the owed rename landed; what remains is item 9

The rename this design carried as "owed post-merge" — `AbortOnDrop` and
the `finish_exec_data_task` abort parameter naming
`tokio::task::AbortHandle` — landed as `04cdf6fa`: three annotations
onto the seam aliases (`AbortOnDrop(TaskAbortHandle)`, the abort
parameter, and `spawn_monitor_subscriber`'s `TaskHandle<()>` return),
no behaviour change, 11 of the 14 RTEMS errors the module reported
closed by it. The direct tokio-handle namings that remain in the
workspace are all in host-only modules — `server_native/{udp,runtime}.rs`
(cfg-gated off RTEMS), `epics-ca-rs`'s hosted async server
(`server/tcp.rs`), and the PVA gateway's `channel_cache.rs` — which is
item 9's sweep (§6), its scope unchanged by item 5's close.
