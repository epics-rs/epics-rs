# Connection worker pool: closing the server-side per-connection thread leak

Status: design, written before the implementation it describes.
Scope: `epics-base-rs` `runtime::worker_pool` (new), `runtime::blocking_io`
(both pumps), `epics-pva-rs` `server_native::blocking` (the connection
thread), and the two blocking-client dial sites that reach the pumps.

**Naming note (2026-07-25).** The target IOC binaries were later renamed —
`rtems-ca-ioc` → `realtime-ca-ioc`, `rtems-pva-ioc` → `realtime-pva-ioc`.
Every old name below is left exactly as captured, because this file is a
record of the tree as it stood, not a description of it as it stands.

## 1. The defect

Every `std::thread` **creation** leaves 176–179 B behind permanently on
RTEMS 6: the thread's TLS key is freed before the key's destructor has run,
so the value block is never reclaimed. Measured on target; recorded in
`doc/upstream-rtems-bugs`. The cost is per *creation*, not per live thread,
so anything that creates a thread per connection leaks without a ceiling —
a client that connects and disconnects in a loop drains the target's heap
at 176 B a cycle for as long as the IOC runs.

`DialPool` (`runtime::blocking_io`, commits 05c7ffed / 9daff491) closed the
**client dial** path by that argument. Three server-side sites still create
a thread per accepted connection:

| # | site | threads per connection | stack class |
|---|---|---|---|
| A | `epics-pva-rs/src/server_native/blocking.rs:807` `PVAS-conn {peer}` | 1 | `Big` |
| B | `epics-base-rs/src/runtime/blocking_io.rs:257` `spawn_pump` (reader + writer) | 2 | `Small` |
| C | `epics-ca-rs/src/server/blocking.rs:334` + `:888` (`CAS-client-blocking`, `CAS-event-blocking`) — **now converted, §9** | 2 | `Big` + `Medium` |

A + B together are the PVA server's **three threads per connection**. B is
also reached by the two blocking *clients* through `drive_socket_blocking`
(`epics-ca-rs/src/client/transport.rs:761`,
`epics-pva-rs/src/client_native/server_conn.rs:388`), so a client reconnect
loop leaks through the same function. C is the same defect at a site the
original report did not name; §9 states why it is sequenced separately and
what decision it needs.

### Anchor audit

- **Anchor:** `rg -n 'spawn_dedicated_thread|thread::Builder|thread::spawn' --glob '*.rs' crates/`
- **Sites (production, per-connection):** `epics-pva-rs/src/server_native/blocking.rs:807`;
  `epics-base-rs/src/runtime/blocking_io.rs:257`;
  `epics-ca-rs/src/server/blocking.rs:334`, `:888`
- **Same defect, fixed in this change:** the first two (and therefore both
  blocking clients, which reach `spawn_pump` through
  `drive_socket_blocking`)
- **Same defect, sequenced (§9), since converted:** the two CA-server sites
- **Distinct, skip:**
  - `epics-base-rs/src/server/status_pv.rs:291` — one pusher thread for the
    life of the IOC, not per connection.
  - `epics-bridge-rs/src/qsrv/group_pump.rs:403` — one pump per *group*,
    created at IOC init.
  - `epics-bridge-rs/src/bin/rtems-pva-ioc.rs:716`,
    `epics-pva-rs/.../blocking.rs:2182,2553` (accept / UDP loops) — one per
    server, created at start-up.
  - `DialPool::dial`'s own `spawn_dedicated_thread`
    (`blocking_io.rs:439`) — already bounded by `MAX_DIAL_WORKERS`.

## 2. What the pool is *not* for

Two claims that would be wrong, stated so the next reader does not have to
re-derive them:

* **It does not raise the concurrent-connection ceiling.** The ceiling is
  per-connection *memory*: 1,589,554 B measured per PVA connection on
  `armv7-rtems-eabihf`, against a 142-connection fd/socket-zone bound. A
  pooled connection occupies exactly the same three stacks while it is
  live, because it is the same three threads doing the same work. What the
  pool removes is the *residue of the creation*, not the *occupancy*. The
  lever on occupancy is `StackSizeClass`, and it is a different change.
* **It is not a general work queue.** A pooled worker runs one job for a
  whole connection, minutes or days long. Nothing else may be queued behind
  it, which is why admission **refuses** (§5) rather than queueing.

Its two jobs are: kill the per-creation residue, and give connection
admission a single owner that can say no.

## 3. Why `DialPool` is not extended and a second primitive is written

They differ in the property that decides the data structure.

| | `DialPool` | `WorkerPool` (this design) |
|---|---|---|
| unit of borrow | one worker, one short job | a **set** of N workers, for the connection's life |
| over capacity | the request **queues**; the caller's own timeout still bounds it | the connection is **refused** with `EAGAIN` |
| job duration | one `connect`, bounded by the OS ladder | unbounded — the connection |
| stack classes | one (`Small`) | heterogeneous *within a set* (`Big` + `Small` + `Small`) |

Merging them would give one type two admission policies selected by
context — the dual meaning this workspace's rules exist to prevent. What
*is* carried over from `DialPool` is every settled design point (§4): a
fixed set of persistent threads, `Mutex` + `Condvar`, count the **busy**
slots and never the parked ones, release the slot **before** the reply, and
scope the pool per role.

## 4. Design points inherited from `DialPool`

1. **Persistent threads, created lazily up to a cap.** A worker set that
   exists never retires while its pool lives. After the first connection at
   each concurrency level there is nothing left to create, so the residue is
   `cap × N × 178 B` **once**, whatever the connect/disconnect cadence.
2. **`Mutex<state>` + per-slot `Condvar`.** No async anywhere: this
   primitive is what the async-free drivers are built on.
3. **Count busy, not parked.** `DialPool` documents why: a worker between
   its job and its park is neither busy nor parked, and counting parked
   workers makes it read as unavailable, so a caller woken by that very
   worker creates a second one it does not need. Here the same rule appears
   as: a set is *idle* only when it is physically in the idle deque, and it
   is put there by the same locked step that clears its busy flag.
4. **Release before the reply.** The set is pushed back to idle *before*
   the worker parks again, so a connection admitted by that push can hand
   the worker its next job immediately; the worker then finds the job
   already in its inbox and never parks. Not a race — the job is deposited
   under the same lock the worker re-reads before waiting.
5. **Per role, not per client.** The PVA server owns its pool; each
   blocking client crate owns one `static` pool for its circuits. Roles do
   not share, because a role's threads carry that role's EPICS priority
   band and stack classes.

## 5. Admission: one owner, `EAGAIN`

`WorkerPool::acquire()` is the **only** gate on starting a connection.

```
acquire() -> io::Result<(SetLease, [Worker; N])>
  idle set available            -> take it
  none, and created < capacity  -> create one set (N threads)
  none, and at capacity         -> Err(ErrorKind::WouldBlock)   // EAGAIN
  a thread could not be created -> Err(that io::Error)          // capacity unchanged
```

Two distinguishable errors, because the operator needs to tell them apart:
`WouldBlock` is "this server is full", any other is "this target is out of
thread resources".

For the PVA server the capacity **is** `PvaServerConfig::max_connections`
(default 1024). The pre-existing `active >= max_connections` check is
deleted rather than kept alongside — two independent limits on one
quantity is the dual meaning §3 rejects. `active_connections()` stays, as a
*report*, which is all it ever was.

## 6. The worker set: an atomic borrow, by construction

A PVA connection needs **three** workers *together*: the connection thread
plus a reader pump plus a writer pump. If it could borrow two and block
waiting for the third, a server at capacity would deadlock — every
connection holding two and waiting for one nobody will release. So the unit
of allocation is the **set**, and there is no API that borrows a worker on
its own:

```rust
pub struct WorkerRole { pub suffix: &'static str,
                        pub stack: StackSizeClass,
                        pub priority: ThreadPriority }

pub struct WorkerPool<const N: usize> { /* roster: [WorkerRole; N] */ }

// The whole borrow, or nothing.
pub fn acquire(&self) -> io::Result<(SetLease, [Worker; N])>
```

The roster is heterogeneous, which is why a set (rather than N draws from N
per-class pools) is the right unit: the PVA roster is
`[conn: Big, reader: Small, writer: Small]`, and drawing from three pools
would reintroduce exactly the partial-borrow deadlock.

Two one-shot facts hold **by type**, not by review:

* `Worker::run(self, …)` consumes the token, so one worker cannot be given
  two jobs.
* `acquire` hands out the array once; there is no second way to obtain a
  `Worker`, so a role cannot be double-booked and a connection cannot use a
  worker it did not lease.

`Worker` and `SetLease` are **not** generic over `N` — only the pool and
the returned array are. A `Worker` therefore crosses into
`spawn_reader_pump` and `drive_socket_blocking` without spreading a const
parameter through every signature.

## 7. Return: symmetric accounting through one owner

A set goes back to idle when **both** of these hold:

* its `SetLease` has been dropped (`leased = false`), and
* every job dispatched on it has returned (`running == 0`).

`running` is incremented only by `Worker::run` (the actor that really
dispatched) and decremented only by the worker loop after the job's closure
has fully returned *and* its captures have been dropped (the actor that
really finished). No side path — teardown, panic, shutdown — touches it.
Both `SetLease::drop` and the worker loop call one function,
`PoolCore::release_step`, which performs the transition under the set lock,
decides `became_free` once (guarded by a `parked` flag, so a double push is
unrepresentable), and only then takes the pool lock to push the set and
decrement `busy`. The two locks are never held at once.

This is what makes a forgotten join *safe* rather than merely unlikely: a
set whose writer pump is still running is not idle, so it cannot be handed
to a second connection even if the connection thread returned without
joining it.

### Where the lease lives

The lease is RAII and the holder is whoever owns the connection's lifetime:

* **PVA server** — the lease is dispatched *with* the connection job; the
  worker loop holds it for the duration. That is what puts `catch_unwind`
  **inside** the return guard (§8).
* **Blocking clients** — `drive_socket_blocking` has no worker of its own
  (the protocol runs on the caller's task), so the lease is held by the two
  returned adapters through an `Arc<SetLease>`. When both are dropped the
  lease drops, and the pump guards — which are declared before it — have
  already joined their jobs.

Neither holder can return a set early, because §7's `running` counter is
the gate in both cases.

## 8. Panics

The worker loop is:

```
loop {
    let job = slot.take_job();          // parks on the Condvar
    let _return = SetReturnGuard { … }; // RAII: release_step on drop
    let outcome = catch_unwind(AssertUnwindSafe(job));
    slot.complete(outcome.is_err());    // wakes a joiner
}                                       // _return drops here
```

* **`catch_unwind` sits inside the RAII return guard.** A connection that
  panics still returns its workers, and it returns them through the same
  single owner as a clean one — no cleanup written on an error branch.
* **The worker survives a caught panic and keeps serving.** The job's state
  lives entirely in the job's closure, which the unwind has already
  dropped; the worker thread itself holds nothing the panic could corrupt.
  Retiring the worker instead would let one repeatedly-panicking client
  drain the pool to zero and leave the server refusing everyone forever —
  a worse failure than the one being handled.
* **A lost worker is never recreated.** The pool creates threads in exactly
  one place, `acquire`'s growth step, bounded by capacity. There is no
  respawn path anywhere: if a worker thread is ever lost, the pool's
  capacity permanently shrinks and admission tightens. It never answers a
  loss by creating another thread, because creating threads is the thing
  this design exists to bound.
* **A panicked job is announced.** `Job::join` returns
  `thread::Result<()>`, and the pump guards keep reporting an `Err` through
  `pump_thread_lost`, which reaches `errlog` whatever the log configuration
  is. A connection job that panics with nobody joining it is announced by
  the worker loop itself.

## 9. The CA server (site C) — RESOLVED: capacity is the fd wall **minus one**, 141

**Status: converted, then corrected by measurement.** This section was
written before the CA sites were done and recorded the decision they needed;
the decision was taken (capacity = the fd wall, 142) and the sites pooled.
The on-target run —
[`doc/rtems-ca-worker-pool-on-target-measurement.md`](rtems-ca-worker-pool-on-target-measurement.md)
— then falsified the argument for that number, in the direction its author
did not anticipate, and the capacity is now **141**. What follows is the
question as it stood, the answer that was taken, what the target said about
it, and the correction.

### The question

`BlockingCaServer` deliberately has **no** connection limit — C `rsrv` has
none either, refusing only on a resource failure and never on a count
(`caservertask.c:110-118`, `:1234-1250`), and `active_connections()` says so
in its own doc comment. A pool has a capacity, so converting CA means
choosing one. The two candidates as originally stated:

1. Capacity well above the measured 142-connection fd/socket-zone ceiling
   (say 256), so the fd layer still refuses first and the count limit is
   never the binding one. Preserves C parity in behaviour; the number is
   arbitrary.
2. Capacity as a new, documented CA connection limit. Honest, but a
   deviation from `rsrv`.

### The decision as first taken — the capacity **is** the fd wall, 142

**256 is rejected.** It is not merely arbitrary, it is *unreachable*: the
143rd client cannot be accepted at all, so a 256-set pool could never lease
its 143rd set. Its `EAGAIN` arm would be dead code, `worker_count()` would
never approach its bound, and §12's verification ("at capacity, `acquire`
returns `WouldBlock` and creates no thread") would be unassertable on the
target. A bound nothing can reach does not bound anything. *(This half of
the argument survives the measurement; the next half does not.)*

Option 2 is rejected for the reason already stated: it is a count limit C
has not got.

That gave `CAS_CLIENT_POOL_CAPACITY = 142`, the measured fd wall itself,
with the claim that setting it *at* the wall is what keeps the `EAGAIN` arm
from being dead code.

### What the target said — the EAGAIN arm is unreachable at 142 too

Measured, §3 of the measurement doc: 142 clients established, all held,
`FD_CNT = FD_MAX = 150` with **zero descriptors free**, `SETS = CAP = 142` —
and `REFUSED` **0**, for the whole run. The 143rd client's `accept` failed
`ENFILE`, so `acquire` was never called and the pool's refusal never ran.

The peer got **nothing**: "the guest accepted nothing and sent nothing — the
host's `recv` returned end-of-stream with zero bytes, no `VERSION` reply".

So capacity 142 has the same defect 256 had, for the same reason: *both*
walls are at 142 and the fd one is always first. The refusal path is
unreachable at any capacity ≥ the wall, not just at capacities above it.
That matters more than a dead code arm, because it means the contract
`refuse_client` documents — refuse after `accept`, tell the peer with
`CA_PROTO_ERROR`/`ECA_ALLOCMEM`, log to the console — **can never execute at
the wall**, which is the one place it was written for. The silent close it
exists to remove is exactly what a client at the wall still sees.

### The correction — `CAS_CLIENT_POOL_CAPACITY = 141`

**A refusal that happens after `accept` needs a descriptor to happen on.**
The server must therefore stop one short of the wall and keep that
descriptor in hand:

| clients held | process descriptors | client #142 dials |
|---:|---|---|
| 142 (capacity = wall) | 150 of 150, none free | `accept` fails `ENFILE`; peer told nothing |
| **141 (capacity = wall − 1)** | **149 of 150, one free** | `accept` **succeeds** on the last one; `acquire` → `WouldBlock`; `refuse_client` sends `ECA_ALLOCMEM` and closes, returning the descriptor |

The accept loop is single-threaded, so a burst of refusals is served one at
a time through that one spare rather than racing for it. The cost is one
concurrent client — 141 instead of 142.

| term | value | source |
|---|---|---|
| descriptors configured | 150 | `crates/epics-rtems-boot/csrc/rtems_config.c` §F |
| held by the IOC at idle | 8 | measured; `FD_CNT` = 8, `FD_FREE` = 142 |
| **fd wall** | **142** | 150 − 8; measured `FD_CNT = FD_MAX = 150`, `CA_CONN_CNT = 142` |
| descriptors per CA client | 1 | `FD_CNT + FD_FREE = 150` at every row of the ramp |
| set stack (`Big` + `Medium`) | 1,572,864 B | 1,048,576 + 524,288; `StackSizeClass::bytes` is `f × 0x10000 × size_of::<usize>()`, `usize` = 4 on `armv7-rtems-eabihf` |
| **measured cost per client set** | **1,591,854 B** | measurement §3.1: (254,509,936 − 28,466,624) / 142; the 18,990 B over the stack pair is allocator overhead and per-connection buffers |
| free heap at idle | **231,289,888 B** | measurement §4, this image |
| **memory wall** | **≈145** | 231,289,888 / 1,591,854 |
| **capacity** | **141** | fd wall − 1 |

`capacity = (fd wall) − 1 = 141`, and `141 < 145`, so memory does not bind.

**The memory numbers moved and the conclusion did not.** The first
derivation used 241,199,000 B free at idle and 1,589,000 B per connection
from `doc/rtems-fd-ceiling-deviation.md`, giving a memory wall of 151. This
image measures 231,289,888 B free and 1,591,854 B per set, giving ≈145. Both
are above the fd wall, so the binding term is the same one either way — but
the margin is 3 sets, not 9, and at the wall the guest had only 5,246,576 B
of free heap left. Anyone tempted to raise the fd cap (the `-D` route in
`doc/rtems-fd-ceiling-deviation.md` §5) should note that memory binds at
about 145 on this image, so raising the cap buys ~4 clients, not 9.

### What 141 buys and what it does not change

* **The documented refusal can execute.** That is the whole reason for the
  −1, and it is the only property the change adds.
* **Creations are bounded**: 141 × 2 = **282** threads for the life of the
  process, ≈ 50 kB of RTEMS TLS residue *once*, against a per-accept leak
  with no ceiling. That is the point of the conversion, and it is measured:
  a 30-cycle serial connect/disconnect ramp added **zero** sets and zero
  workers (measurement §2), and 284 distinct `PRIOPROBE` labels over the
  whole run confirm no worker was ever created twice.
* **The refusal a client meets past 141 is still a resource refusal, not a
  count.** 141 is derived from the descriptor budget and moves with the fd
  cap; §9's rejected option 2 was a limit independent of the resource. The
  client that would have been #142 is not turned away silently — it is
  accepted and answered.
* **It does not raise the connection ceiling** (§2), and it lowers the
  served ceiling by exactly one.

### Deviations this introduces

* **The pool never shrinks.** After a peak of *n* concurrent clients the
  process keeps *n* sets — *n* × 1,591,854 B measured — for its whole life,
  where the per-accept driver returned each client's stacks on disconnect.
  This is measured, not projected: after a 142-client peak fully
  disconnected, **224,657,080 B stays allocated with zero clients connected**
  and free heap ends at 2.87 % of its idle value (measurement §4). At
  capacity 141 the corresponding figure is 141 × 1,591,854 ≈ 224.4 MB on a
  256 MB guest. This is the price of removing the creation residue and it is
  bounded by the high-water mark, which is exactly what that peak already
  cost while it was live; there is no load at which the pooled driver needs
  more memory than the un-pooled one *needed at its worst moment*. What it
  does change is that the IOC does not get that memory back afterwards, so a
  single burst permanently sizes the process.
* **The pool is process-wide (`static`), not per-server.** The resource it
  bounds — descriptors, stacks, heap — is process-wide, the same argument
  `refused_clients()` already makes for its counter, so two servers in one
  process must share one bound rather than have 141 each. It also removes a
  teardown ordering CA cannot state: `WorkerPool::drop` joins every worker,
  and a worker inside a live client takes its `Stop` only on disconnect, so
  a server-owned pool would need a registry of live sockets to shut first
  (what the PVA server's `ConnRegistry` is). A `static` pool is never
  dropped.
* **Thread names lose the peer**, as §11 records for every pooled role:
  `CAS-client 3`, not `CAS-client 10.0.0.1:44312`. The measurement records
  in `doc/rtems-priority-on-target-measurement.md` and
  `doc/rtems-pi-step7-target-measurement.md` name the old
  `CAS-client-blocking <peer>` / `CAS-event-blocking <peer>` threads; those
  are historical measurements and are left as they were taken.

### What is no longer true above

§1's site table lists site C as open, and §1's "Same defect, sequenced (§9)"
line follows from that. Both are now closed: `blocking.rs` creates **no**
thread per client, and its second refusal site — the event-thread creation
failure inside the connection body, which used to fire *after* the VERSION
frame had gone out — is gone rather than handled, because admission moved up
to the accept loop's single `acquire`.

**The 176–179 B residue itself** may be removed by a target-spec flip (a
separate panel). That does not remove the need for this pool: admission
with a single owner, and the fd/memory ceiling being reached by refusal
rather than by an unbounded spawn, are independent of whether a creation
leaves residue behind.

## 10. Instrumentation

Mirroring `DialPool::worker_count` / `queue_depth`:

* `worker_count()` — threads this pool has created, ever. The number the
  per-connection shape grew without limit; the bound made observable.
* `set_usage() -> (busy, created, capacity)` — the admission state.
  Deliberately not called `queue_depth`: there is no queue (§3), and a
  function named for one would be the dual meaning again.

## 11. Deviations introduced

* **Thread names lose the peer.** A pooled thread is named once, at
  creation: `PVAS-conn 3`, not `PVAS-conn 10.0.0.1:44312`. A target thread
  census now says how many connections are being served but not by whom.
  Same deviation `DialPool` took, for the same reason, and the
  operator-facing `PumpSpec::label` — which is what a loss report prints —
  is still per-connection.
* **`drive_socket_blocking` no longer takes two priorities.** They are
  properties of the pool's roster now, declared once per role rather than
  passed per connection. The CA client's asymmetric pair (receive band
  below send band, `tcpiiu.cpp:677-682`) is expressed in its roster.
* **`spawn_reader_pump` / `spawn_writer_pump` become infallible.** They no
  longer create a thread, so there is no creation to fail; the failure they
  used to report has moved to `acquire`, which is where admission belongs.

## 12. Verification

* `worker_count()` after K sequential connect/disconnect cycles is
  `N`, not `K × N` — the direct statement of the closed leak, and the
  assertion is inside the loop as well as after it, because the tight spot
  is the connection immediately after a release (§4.3).
* A connection whose job panics returns its set: the next `acquire`
  succeeds and `worker_count()` is unchanged.
* At capacity, `acquire` returns `WouldBlock` and creates no thread. **A
  unit test asserting this proves nothing about the target**: on the target
  the capacity is only reached if some *other* resource does not bind first.
  The CA measurement is the case in point — at capacity 142 the descriptor
  cap bound first, `accept` failed `ENFILE`, and `REFUSED` stayed 0 for the
  whole run with `SETS = CAP = 142`
  ([`doc/rtems-ca-worker-pool-on-target-measurement.md`](rtems-ca-worker-pool-on-target-measurement.md)
  §3). So the on-target form of this check is: hold `capacity` connections,
  dial one more, and require that the peer is *answered* — a refusal frame,
  not an end-of-stream — and that `REFUSED` incremented. For a socket
  server that means the capacity must leave a descriptor for the refusal to
  happen on (§9's `fd wall − 1`); a capacity at or above the wall makes this
  bullet untestable rather than true.
* A set is not reused while any of its jobs is still running.
* Concurrency tests are run 50× in isolation and the pass ratio reported —
  the exec-backend class shows ~90% green on a single run, so one green run
  is not evidence.
