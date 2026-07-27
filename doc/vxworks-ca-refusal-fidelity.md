# CA connection refusal: what the operator is told, and what the peer is told

The E8 on-target run on `x86_64-wrs-vxworks` measured a CA server that refuses
clients correctly and *reports* the refusal incorrectly, in two places at once.
This document records the family, the structural cause of each half, the fix,
and the on-target verification. Rig: `~/vx-rig-e11` on the shared build box,
QEMU x86-64 VxWorks 7 guest, `realtime-ca-ioc` RTP.

## 1. The family

One sentence: **a refusal outcome is not faithfully reported — neither to the
operator nor to the peer.**

Two measured halves:

* **The operator is told a number that is not the refusal count.** Refusals were
  announced on `errlog` only when the ordinal was a power of two, while an
  ungated `tracing::warn!` sat beside them. Eight refusals produced four
  `errlog` records; four produced three.
* **The peer is told a status that cannot distinguish the two admission
  gates.** `available` is `ECA_ALLOCMEM` (48) whether the pool was at capacity
  or the OS refused to create the thread — two different operational problems
  with two different remedies.

Refusals are exactly what an operator reads when an IOC stops accepting
clients, so an understated refusal stream is the failure mode that matters.

## 2. Measured, pre-fix, on this box (E8)

`~/vx-rig-e8/console-fix-2048M.log:525-531`, the pool-capacity gate — note the
missing `errlog` line for `nth=3`:

```
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (worker pool at capacity) — refused 10.0.2.2:39296 (refusal #1)
WARN  epics_ca_rs::server::blocking: refused a CA client for want of resources peer=10.0.2.2:39296 error=worker pool at capacity nth=1
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (worker pool at capacity) — refused 10.0.2.2:39304 (refusal #2)
WARN  epics_ca_rs::server::blocking: refused a CA client for want of resources peer=10.0.2.2:39304 error=worker pool at capacity nth=2
WARN  epics_ca_rs::server::blocking: refused a CA client for want of resources peer=10.0.2.2:39316 error=worker pool at capacity nth=3
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (worker pool at capacity) — refused 10.0.2.2:39322 (refusal #4)
WARN  epics_ca_rs::server::blocking: refused a CA client for want of resources peer=10.0.2.2:39322 error=worker pool at capacity nth=4
```

`~/vx-rig-e8/console-run1-1024M.log:2774-2780,5468-5472`, the spawn-failure
gate — 8 refusals, `errlog` records for #1, #2, #4, #8 only:

```
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (resource unavailable try again (os error 11)) — refused 10.0.2.2:38034 (refusal #1)
...
WARN  epics_ca_rs::server::blocking: refused a CA client for want of resources peer=10.0.2.2:38044 error=resource unavailable try again (os error 11) nth=3
```

Counted across the five E8 consoles (`errlog` records vs. refusals):

| console | refusals | `errlog` records | lost |
|---|---|---|---|
| `console-run1-1024M.log` | 8 | 4 | 4 |
| `console-run3-2048M-final.log` | 4 | 3 | 1 |
| `console-fix-1536M.log` | 3 | 2 | 1 |
| `console-fix-2048M.log` | 4 | 3 | 1 |
| `console-fix-1024M-einval.log` | 0 | 0 | — |

The loss is **not** inside `errlog`. `errlog_sev_printf` has no ring buffer and
no discard path in this port; it formats and routes synchronously. The records
were never emitted: `refusal_should_be_announced(nth) == nth.is_power_of_two()`
gated the call itself.

## 3. Structural cause

**Half one — two sinks, two emission rules.** The refusal outcome had two
consumers (`errlog`, `tracing::warn!`) and each followed a different rule, so
neither stream is the refusal count on its own and a reader has to reconcile
them. That is the dual meaning; a sampled sink beside an unsampled one is an
edge factory, not a rate limit.

The console-flood justification for the sampler was void by construction: the
`warn!` beside it was ungated and reached the *same* console through
`runtime::log::install_console_subscriber`, so the console already paid one
line per refusal. The schedule suppressed only the second line. One record per
refusal is therefore strictly *less* console traffic than what it replaced
(2 lines/refusal at power-of-two ordinals, 1 elsewhere → 1 uniformly).

C does the same thing this converges on: `rsrv/caservertask.c:1246-1256` prints
one unconditional `epicsPrintf` per refusal, with *different text per gate*
(`below max block thresh` vs `alloc failed`). C throttles the accept loop, never
the message.

**Half two — the gate was a stringly-typed `io::Error`.** `WorkerPool::acquire`
returned `io::Error` for all three admission outcomes. `ErrorKind` cannot
separate them: `std` maps `EAGAIN` to `ErrorKind::WouldBlock`, and the
at-capacity refusal was *also* constructed as `WouldBlock`. Any caller that
branched on `kind()` was branching on a collapsed value. One did:
`epics-pva-rs`'s blocking server logged `max_connections reached` for an
out-of-threads refusal, naming a limit that was not the limit that fired.

## 4. Fix

Two commits, one per finding.

* `fix(ca-server): announce every client refusal exactly once` —
  `refusal_should_be_announced` and the duplicate `tracing::warn!` leg are
  deleted; `refuse_client` ends in a single unconditional
  `errlog_sev_printf(Major, …)` carrying peer and ordinal. `errlog` is the
  surviving sink because it is C's `epicsPrintf`/`errlogPrintf` seam, because it
  reaches the console with no `tracing` subscriber installed (the state an
  embedded IOC binary runs in), and because it is *also* a `tracing` event on
  `epics_base_rs::errlog`, so subscriber-based applications lose nothing.
* `fix(worker-pool): name the admission gate that refused, not an errno` —
  `acquire` returns `Result<_, AcquireError>` with one variant per gate
  (`AtCapacity { capacity }`, `SpawnFailed(io::Error)`, `ShuttingDown`).
  `From<AcquireError> for io::Error` keeps the old shape available for callers
  that only propagate, with the typed cause preserved as `source()`. The CA
  server's `refusal_reason` and the PVA server's log line both branch on the
  variant instead of on `kind()`.

**Invariant.** MUST: each refusal produces exactly one console record, carrying
its ordinal, and names the gate that refused. MUST NOT: any refusal be sampled
away; MUST NOT: two records be emitted that a reader has to reconcile.

**Owner.** `refuse_client` is the sole refusal owner in the CA server; a
structural test asserts one definition and one call site, so an `acquire`
failure that does not reach the owner (and would close the socket in silence)
fails the build's test run rather than the next on-target ramp.

## 5. The wire status is unchanged, and why

`available` stays `ECA_ALLOCMEM` (48) for both gates. The gate travels in the
diagnostic string, which libca prints as the exception `Context:` line.

C rsrv does not constrain this — a refused peer gets **zero bytes** from C
(`caservertask.c:1247`, `:1254`: `epicsSocketDestroy(sock)` then `return NULL`).
libca does. `ca_client_context::vSignal`
(`modules/ca/src/client/ca_client_context.cpp:412-416`) calls `abort()` for any
status that is not success and whose severity is not `CA_K_WARNING`. The only
code that says what the capacity gate means is `ECA_MAXIOC` — "Maximum
simultaneous IOC connections exceeded" — and it is `DEFMSG(CA_K_ERROR, 1)`
(`caerr.h:86`, marked `/* defunct */` upstream). Sending it would crash every
default-handler `caget`/`camonitor` that hit a full IOC. No `CA_K_WARNING` code
means "server full".

So a machine-readable gate on the wire needs a new status code, i.e. a
wire-visible protocol change. That is left open (§8), not silently taken.

## 6. On-target verification

Image: `realtime-ca-ioc.vxe`, `x86_64-wrs-vxworks`, release, features
`client-core,bringup-probes`, md5 `ec006f44966cabdc1d8f89f3a55c580b`. Driver:
`doc/vx-rig-e11/refusalprobe.py` — ramps to the wall, then holds the ramp and
makes further attempts, so the refusal ordinals form one **consecutive run of
8**. Eight is deliberate: a power-of-two-sampled server announces {1,2,4,8} and
hides {3,5,6,7}, so any gap is visible in one console.

### 6.1 Pool-capacity gate — 2048M guest

Wall at ramp attempt 137 (136 ramp + 5 monitor = 141 = pool capacity), 410 s
ramp. All 8 refusals carry the capacity:

```
[  414.2s] e11-2048M WALL attempt=137 held=136 elapsed_conn=3.78s REFUSED_BY_SERVER(status=48 text='CAS: no resources for a new client (worker pool at capacity: 141 concurrent clients)')
```

frame `000b0068 00000000 ffffffff 00000030 …` — `available` = 0x30 = 48,
payload 0x68 = 104 bytes (pre-fix: 0x50 = 80).

Console, `~/vx-rig-e11/console-e11-2048M-capacity.log:139-146` — 8 records,
ordinals 1..8, no gaps, and zero `want of resources` lines:

```
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (worker pool at capacity: 141 concurrent clients) — refused 10.0.2.2:50948 (refusal #1)
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (worker pool at capacity: 141 concurrent clients) — refused 10.0.2.2:50958 (refusal #2)
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (worker pool at capacity: 141 concurrent clients) — refused 10.0.2.2:50970 (refusal #3)
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (worker pool at capacity: 141 concurrent clients) — refused 10.0.2.2:50986 (refusal #4)
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (worker pool at capacity: 141 concurrent clients) — refused 10.0.2.2:50998 (refusal #5)
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (worker pool at capacity: 141 concurrent clients) — refused 10.0.2.2:51008 (refusal #6)
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (worker pool at capacity: 141 concurrent clients) — refused 10.0.2.2:51014 (refusal #7)
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (worker pool at capacity: 141 concurrent clients) — refused 10.0.2.2:51030 (refusal #8)
```

Spot-check after the wall: 10/10 sampled held connections still answer a fresh
`READ_NOTIFY`, so this is a refusal, not a collapse.

### 6.2 Spawn-failure gate — 1280M guest

Wall at ramp attempt 92 (91 ramp + 5 monitor = 96). The gate is named, and the
errno is kept as its cause rather than standing in for it:

```
[   78.5s] e11-1280M WALL attempt=92 held=91 elapsed_conn=7.25s REFUSED_BY_SERVER(status=48 text='CAS: no resources for a new client (cannot create a client thread: resource unavailable try again (os error 11))')
```

Console, `~/vx-rig-e11/console-e11-1280M-spawnfail.log:90-100` — again 8
records, ordinals 1..8, zero `want of resources` lines. Spot-check 10/10.

Pre-fix, this gate read `(resource unavailable try again (os error 11))`: an
errno with no gate. Post-fix the two gates are distinguishable by text on the
wire and in the log; `available` is 48 in both, as §5 requires.

### 6.3 The 1024M guest never reaches the refusal path (pre-existing)

At 1024M the RTP aborts at 41 held connections before any refusal:

```
memory allocation of 64 bytes failed
memory allocation of 80 bytes failed
skipping backtrace printing to avoid potential recursion
0xffff800010336000 (CAS-client 46): RTP 0xffff8000096ac000 has been deleted due to signal 6.
```

This is **not** caused by the fix. Control: `origin/main` (both commits
reverted), same three source files, same feature set, same toolchain, built in
the same target dir, run against the same probe on a fresh 1024M guest —
identical outcome, held=41, attempt 42, same allocator abort, same
`CAS-client 46`
(`doc/vx-rig-e11/refusalprobe-base1024.log`,
`~/vx-rig-e11/console-e11-1024M-BASELINE-allocabort.log`). Reproduced twice
with the fixed image and once with E8's own `phaseramp.py` driver, so it is
deterministic and driver-independent.

It differs from E8's 1024M numbers (held=43, mutex `EINVAL` panic, IOC
survives) because E8's image carries that branch's extra on-target probes;
`origin/main`'s image aborts two connections earlier and does not survive. The
1-in-3 wall-abort `EINVAL` named in `doc/vxworks-port.md` §7 did **not**
reproduce here in four wall events; what reproduced is an allocator abort,
which is a distinct failure mode at the same wall. Neither is in this family's
scope; recorded here because it was measured.

## 7. Host regression tests

* `every_refusal_produces_exactly_one_console_record` — drives 7 refusals
  through `refuse_client` under a scoped subscriber that counts events, asserts
  7 `errlog` events and 0 elsewhere. Fails on the pre-fix tree (3 records: the
  {1,2,4} schedule) and on any tree that reintroduces a second sink.
* `each_admission_gate_names_itself_and_its_remedy_in_the_refusal` — asserts
  the two gates produce different diagnostic text on the wire, and that each
  text names its gate. Fails on the pre-fix tree for the spawn gate, whose text
  was the bare errno.
* `a_full_pool_and_a_refused_spawn_are_not_the_same_refusal` — asserts
  `io::Error::from_raw_os_error(11).kind() == WouldBlock` (the collapse that
  made the two gates indistinguishable), that the two `AcquireError` variants
  are distinct, and that the cause survives the `io::Error` conversion by
  downcast.
* `there must be exactly one refusal owner` (structural) — one definition, one
  call site of `refuse_client`.

## 8. Left open

* **The wire status still collapses the two gates, and this is a protocol
  limit rather than a deferred fix.** `available` = 48 for both. The CA status
  vocabulary contains no code that can safely say *server full*: any status
  whose severity is not `CA_K_WARNING` calls `abort()` in a default-handler
  client (`ca_client_context.cpp:410-416`), and the one code that means what the
  capacity gate means — `ECA_MAXIOC` — is `DEFMSG(CA_K_ERROR, 1)` and is marked
  `/* defunct */` upstream (`caerr.h:86`). So the choice is not "48 versus the
  right code"; it is "48 versus crashing every stock `caget` that meets a full
  IOC". Keeping `ECA_ALLOCMEM` and carrying the gate in the diagnostic string is
  the decision (user sign-off, 2026-07-27). Closing it for real needs a new
  `CA_K_WARNING` status code, i.e. a change to the protocol's vocabulary, not to
  this server.
* **`RTEMS:CA_REFUSED_CNT` reads 0 while the wall is loaded.** The status PVs
  starve under the ramp (a known band-189 effect on this target), so the
  server-side refusal counter is not a usable cross-check at the wall; the
  numbers above are console and wire counts.
* **The 1024M allocator abort** (§6.3) is measured and not root-caused. It is
  no longer *reached* on the default configuration — §9's reservation budget
  refuses at 32 sets and the abort needs 46 — but the failure itself is
  untouched: raise `EPICS_RS_POOL_RESERVATION_MB` past the ceiling and it
  returns unchanged (§9.3). The budget bounds the process away from it; it does
  not fix it. Root cause is E10's.
* **The object-arena term of the reservation is unmeasured** (§9.2). Each
  VxWorks pthread mutex materialises a kernel `SEMAPHORE` on first lock, from an
  arena that is neither the address space charged nor the allocator heap — which
  is why the same wall shows as `EINVAL` in one probe and as a failed 64-byte
  allocation in another. The term is named and charged 0 pending E8's
  `semMCreate` wrap; if it turns out to bind first it needs its own budget, not
  a share of this one.
* **The 160 MiB default costs connections on a well-provisioned target.** It is
  the guest this was measured on, not the largest one: at 1470 MB of guest
  memory E8 measured a ceiling ≥739 MB, where the same default still refuses at
  32. The remedy is the switch, and the refusal names it. A budget derived from
  the target's own memory is not possible today — the reserved-address-space
  ceiling is not a fixed fraction of RAM across the sizes measured (25.9 % at
  958 MB, ≥50 % at 1470 MB) and the VxWorks census backend reports `MEM_FREE` as
  NaN.

## 9. The pool walked past the memory ceiling, and now refuses before it

Two defects in `runtime/worker_pool.rs`, closed after §§1–8 and measured on the
same rig. They are one family with §6.3: the IOC had no bound on the *memory*
its connection threads reserve, so every path that ran out — a failed
`pthread_create`, a `std` mutex whose `semMCreate` returned NULL, a failed
64-byte allocation — was reached by walking past the ceiling rather than by
refusing at it.

### 9.1 A worker that dies leaked its set

`catch_unwind` wraps the job body, not the thread. Dropping the panic payload
when the joiner is gone, and both mutex takes in the worker's return path, sit
outside it — and on this target the second is real: a `std` mutex lock at the
wall returns `EINVAL` ("failed to lock mutex: invalid argument (os error 22)")
and `std` panics. The set was then stuck at `running == 1` with no thread left
to settle it: never idle, never leased again, its slot held against the pool's
bound for the life of the process. Measured on the E8 branch as `BUSY=2 SETS=50
WORKERS=100 CONNS=0` — two sets held with no client attached.

The accounting now hangs off the thread's destructor (`WorkerExit`), so any exit
that was not asked for marks the set dead, stops its siblings, and returns the
slot when the last of its threads is gone. A dead set is never pooled again, and
a dispatch onto a worker already gone is reported as *not run* instead of as a
clean completion.

### 9.2 The bound was a count, and the count was never what ran out

`CAS_CLIENT_POOL_CAPACITY = 141` is a *descriptor* bound (§ the constant's own
derivation). It was never approached on this guest. `Reservation` adds the
bound that was missing: one process-wide account of thread memory, charged per
thread as **declared stack + a flat 1 MiB** (E8's three-arm measurement) with the
RTP object arena named as a third term and not yet charged (`semMCreate` is what
returns NULL at the wall; E8's wrap will give the per-thread figure). A whole
set is reserved before any thread is created, so admission refuses while the
target is still healthy enough to deliver the refusal. Default 160 MiB on an
embedded target, unbounded on a host, raised with
`EPICS_RS_POOL_RESERVATION_MB`.

The CA set is `Big` + `Medium` = 3 MiB declared + 2 MiB overhead = **5 MiB**, so
160 MiB is 32 sets.

### 9.3 Measured: 1024M guest, one image, three budgets

`doc/vx-rig-e11/refusalprobe-poolfix1024.log`,
`doc/vx-rig-e11/refusalprobe-envraise320.log`, and the control
`doc/vx-rig-e11/refusalprobe-base1024.log`.

| budget | sets reached | outcome |
| --- | --- | --- |
| none (`origin/main` control) | 46 (41 ramp + 5 monitor) | `memory allocation of 64 bytes failed`, signal 6, RTP deleted, **zero refusals** |
| 320 MiB (`putenv` in the kernel shell) | 46 | identical death at `CAS-client 46`, zero refusals |
| 160 MiB (the default) | 32 (27 ramp + 5 monitor) | **8 consecutive refusals, IOC alive** |

At the default the whole run takes 1.4 s and ends with `spot-check: 10/10
sampled held connections still answer a fresh READ_NOTIFY`. Every refusal
carries `status=48` and the gate in the text:

```
CAS: no resources for a new client (worker pool at its thread-memory budget:
this set needs 5120 KiB, 160 of 160 MiB already reserved — raise
EPICS_RS_POOL_RESERVATION_MB if the target has the memory)
```

and one `errlog` record each, ordinals `#1`–`#8` consecutive
(`~/vx-rig-e11/console-e11-1024M-POOLFIX-refuses.log`), which is §4's
one-record-per-refusal rule holding on a gate that did not exist when it was
written.

The 320 MiB row is what makes the default's number a measurement rather than a
guess: the switch reaches the RTP (the wall moved from 32 to 46), and raising it
past the ceiling walks straight back into §6.3's abort at the same set 46. So
the fatal point on this guest is ~230 MiB of pool reservation, and 160 MiB sits
14 sets below it. On a larger guest the ceiling is higher (E8: ≥739 MB at 1470
MB of guest memory) and the same switch is how an operator gets those
connections back — the cost of the default is that a well-provisioned target
serves 32 CA clients until it is raised.

### 9.4 Host regression tests

* `a_worker_that_dies_retires_its_set_instead_of_leaking_it` — kills a worker
  thread at exactly the point the target killed one (the payload drop after the
  joiner is gone), asserts the set stops being busy, that `created` returns to
  0, and that a fresh borrow still runs. Fails on the pre-fix tree by timeout:
  the set stays busy forever.
* `a_death_under_a_live_lease_returns_the_slot_and_never_repools_the_set` — the
  other side of the boundary: the borrower still holds its lease. Asserts the
  slot comes back anyway, that a dispatch onto the retired set reports *not
  run*, and that the lease drop does not re-pool it.
* `admission_refuses_at_the_memory_budget_before_the_count_bound` — a budget of
  exactly two sets against a capacity of eight: two admitted, the third refused
  with the three numbers the remedy needs, no thread created, and the account
  back to zero when the pool drops. Fails with the gate removed.
* `a_dead_set_gives_its_memory_back_to_the_budget` — a budget of exactly one
  set, whose worker then dies; the memory must return or the target refuses
  connections it has the memory to serve. Fails with the release removed.
* `the_reservation_budget_reads_its_switch_and_its_target_default` — both arms
  of both target-dependent constants and every switch-parse case, on a host.

## 10. Can the budget be derived from the target instead of a constant?

The 160 MiB default of §9.2 is correct for the box it was measured on. A 1470 MB
guest holds ≥739 MB and still refuses at set 32, so the constant undersells a
larger target. C solves the same problem with a live query rather than a
constant — `osiSufficentSpaceInPool` polls `memFindMax()` on vxWorks and
`malloc_free_space()` on RTEMS — so the question is whether that query is
available to us. It is not. What is available is a *probe*, and the difference
between the two is what decides the design.

### 10.1 What an RTP can be asked, and what it answers

Every candidate links: `memFindMax`, `memPartInfoGet`, `memInfoGet`,
`memPartFindMax`, `sysctl` and `rtpInfoGet` are all `T` in the RTP `libc.a`.
Linking is not answering. Measured on target (`doc/vx-rig-e11/memquery.c`,
`doc/vx-rig-e11/adrspace-survey.log`):

| asked | answer |
| --- | --- |
| `sysctl` `CTL_HW`/`HW_PHYSMEM`, `HW_USERMEM`, `HW_PAGESIZE` | `ENOENT` |
| `sysctl` `CTL_KERN` 41/42 (`memtop`, `physmemtop`) | `ENOENT`; and `sysctlCommon.h` withdraws both names under `_WRS_CONFIG_LP64` |
| `memFindMax()` | 261,744 B, flat from the first thread to the wall |
| `memInfoGet()` | free 261,760 / maxfree 261,744 / alloc 0, flat likewise |
| `sysconf(_SC_PHYS_PAGES)` | the constant does not exist in the RTP `unistd.h` |
| `getrlimit`/`setrlimit` | defined in no library under `sysroot/usr/lib/common` |
| `rtpInfoGet()` | `RTP_DESC` carries status, options, entry, ids, path, task count and text bounds — no memory field |

`memFindMax` describes a 256 KiB heap partition that never moves while the
process reserves a quarter of a gigabyte. That is the trap named up front: the
binding resource is reserved address space, not free heap, and every heap
question is blind to it. The target says so itself — `pthread_create` fails with
`errno = 0xB4000E`, which is `M_adrSpaceLib` (module 180) and not a `memLib`
code. base's `vxWorks/osdPoolStatus.c` is not wrong; it runs in the kernel,
where `memFindMax` is the system heap. We run as an RTP, where it is not.

### 10.2 What a probe can measure, exactly

Taking, unlike asking, works. An `mmap(PROT_NONE)` ladder run to exhaustion and
released reports a ceiling that equals the `pthread_create` wall **to the byte**,
at three stack classes and two guest sizes:

| guest | OS memory | stack | mmap chunks | mmap total | pthread wall | reserved at wall |
| --- | --- | --- | --- | --- | --- | --- |
| 1024M | ~958 MB | 2 MiB | 127 | 266,338,304 | n=127 | 266,338,304 |
| 1024M | ~958 MB | 1 MiB | 254 | 266,338,304 | n=254 | 266,338,304 |
| 1024M | ~958 MB | 512 KiB | 509 | 266,862,592 | n=509 | 266,862,592 |
| 1536M | ~1470 MB | 2 MiB | 382 | 801,112,064 | n=382 | 801,112,064 |

Two laws fall out. The ceiling is a byte ceiling, not a thread count — it holds
within one chunk (0.2 %) across a 4× change in stack size. And it moves 1:1 with
guest memory: 958 − 254 MiB ≈ 704 MB, 1470 − 764 MiB ≈ 706 MB, so the target
keeps a fixed ~705 MB and hands an RTP the rest. That is the same line E10
measured from the other end (0.20580 clients per MB of guest RAM, R² = 0.998),
seen at its source.

A third measurement is why this stays a probe and not a policy: a bare pthread
costs exactly its declared stack here, with no flat per-thread term at all — the
ladder totals are `n × stack` in every row above. The flat 1 MiB that
[`per_thread_overhead`] charges is therefore not what the OS charges for a
thread; it is what a *Rust* thread reserves on top of its stack, which is
consistent with a per-thread allocator arena and coherent with E10's abort
landing on a 64-byte allocation inside a freshly spawned thread.

### 10.3 Why the constant stays anyway

The probe is exact, but it is a taking, and both ways of using it cost more than
the constant does:

* Run to exhaustion it holds the entire address space for the duration (~0.5 s
  at 2 MiB chunks), during which any other thread's growth fails — and a failed
  allocation in this process is `abort`, which is the very outcome §9.2 exists
  to prevent. Trading a bounded refusal for a probabilistic abort is backwards.
* Held to a single mapping it is safe but conservative: on the guest whose
  chunked ceiling is 254 MiB, a single 256 MiB mapping fails and 192 MiB
  succeeds — a ≥24 % under-read — and each attempt costs 0.3–0.5 s, so a
  descent is seconds of startup.

So the survey's answer to "is there a queryable quantity that tracks our wall"
is **no**, and the constant plus `EPICS_RS_POOL_RESERVATION_MB` stays. What the
measurement adds is that an operator can now compute the switch rather than
guess it: usable address space ≈ OS memory − 705 MB, a CA set costs 5 MiB, and
the default keeps 14 sets of headroom below the wall.

The second bullet is a cost against *sizing* the budget, not against *vetoing*
one. Asked "is the configured value obtainable" rather than "what is the
ceiling", one mapping is a yes/no and the under-read falls on the safe side — so
that is what boot does with it (§11.2), while the constant stays what an
unconfigured process gets.

### 10.4 RTEMS

`malloc_get_statistics` does not exist in RTEMS 6: it is absent from every
header under the BSP's `lib/include` and from every archive in `lib`. base knows
this — `RTEMS-posix/osdPoolStatus.c` switches on `__RTEMS_MAJOR__ < 5` and uses
`malloc_free_space()` for 5 and later. That function *is* reachable in our
build: declared in `rtems/libcsupport.h`, defined in `librtemscpu.a`.

Whether it tracks an RTEMS wall of ours is still unmeasured — no RTEMS ladder of
the kind §10.2 ran here has been run — but the budget's *runtime* behaviour on
RTEMS is no longer unmeasured, and the earlier reading of it was wrong. This
section previously said the budget was inert on RTEMS, on the arithmetic that
160 MiB is 62.5 % of a 256 MB `xilinx-zynq-a9` guest and that the count bound or
the heap must therefore be reached first. It is not what happens. See §11.3.

## 11. On-target round: the arena gate, RTEMS, and who publishes the wall

Three questions taken to the target on one rig (E11: VxWorks 7 `x86_64` guest on
host ports 51534/55064/55075; RTEMS 6 `xilinx-zynq-a9` guest on 45064/45065).
One of the three answers is a failure and is recorded as one.

### 11.1 The object-arena gate is not demonstrated

`materialise_set_mutex` takes the set's own `Mutex<SetState>` before any thread
of that set exists, so a target that cannot hand out a kernel mutex object
refuses the client instead of killing a worker inside `Mutex::lock`. E8 measured
the failure it is aimed at: `semMCreate=NULL` with 588 live semaphores at 49
sets / 98 workers, transient, so the object arena is a rate problem and not a
total — which is why it has its own gate rather than a term added to the byte
budget.

That failure did not reproduce on this rig, so the gate's effect could not be
shown. Three configurations, all on the gated image:

| guest | `EPICS_RS_POOL_RESERVATION_MB` | what bound first |
|---|---|---|
| 1024M | 320 | `memory allocation of 64 bytes failed` → signal 6, RTP deleted, at attempt 42 |
| 1536M | 400 | budget refusal at attempt 76, IOC survived |
| 1536M | 1200 | `CAS_CLIENT_POOL_CAPACITY` refusal at attempt 137, IOC survived |

No `semMCreate=NULL` in any of them. The 1024M/320 run is the one that matters:
the pre-gate image aborted there at attempt 42 (`console-e11-1024M-envraise320-abort.log`),
and the gated image aborts there at attempt 42 with the identical message. The
gate changed nothing, because the allocation that fails is E10's 64-byte
per-thread TLS destructor list, taken by `std` *inside* the already-spawned
thread — past `pthread_create`, past every gate, and with no fallible
`Vec::push` to fail into. That is the residue §10 already names, and this round
measured that it, not the semaphore arena, is what binds on this box.

So: the gate is shipped and unit-tested, and its on-target effect is
**unverified**. Verifying it needs a run that reaches `semMCreate=NULL` before
the address-space wall, which this rig's ratio of guest RAM to arena size does
not produce.

### 11.2 The abort is reachable only through the operator's own switch

Worth stating plainly, because the table above invites the wrong reading. With
the **default** 160 MiB budget the 1024M guest refuses cleanly at 17 clients and
keeps serving. Every abort in this round required raising
`EPICS_RS_POOL_RESERVATION_MB` past what the target can honour — 320 MiB on a
box whose measured ceiling is ~254 MiB of thread reservation (§10.2). The switch
is documented as "raise it if the target has the memory" and takes the operator
at their word; when they are wrong the process dies rather than refusing.

That is now closed at boot. `decide_reservation_budget` adopts the largest size
the target *answers for*, found by halving from the configured value down to an
8 MiB floor, where the answer is §10.2's single anonymous `PROT_NONE` mapping,
taken and released. One mapping under-reads the chunked ceiling — 192 MiB
confirms on a guest whose ladder reaches 254 MiB — which is the safe direction
for a veto: it can refuse a budget the target would in fact have honoured, never
admit one it would not. Halving is coarse on purpose; an operator who wants a
size between two halvings names it and has that one confirmed.

Measured on this rig, same image, same 1024M guest, `EPICS_RS_POOL_RESERVATION_MB`
varied at the shell:

| configured | adopted | boot line | wall | outcome |
| --- | --- | --- | --- | --- |
| 320 MiB (pre-fix) | 320 MiB | none | — | RTP deleted, `signal 6`, at CAS-client 46 |
| 320 MiB | 160 MiB | `sevr=major … clamped from 320 MiB to 160 MiB` | 18/17 | refuses, keeps serving |
| 160 MiB (= default) | 160 MiB | silent | 18/17 | refuses, keeps serving |
| 4096 MiB | 128 MiB | `sevr=major … clamped from 4096 MiB to 128 MiB` | 12/11 | refuses, keeps serving |

The clamp line lands before `iocInit`, two lines into the RTP's own output. Its
cost is the descent: shell-prompt to `serving` is 14 s at one mapping (160) and
16 s at six (4096 → 2048 → 1024 → 512 → 256 all refused, 128 given), so ≤ ~2 s
for the deepest descent an operator can provoke on this guest, paid once.

What the switch cannot do on RTEMS is be checked at all: no RTEMS ladder has been
run (§10.4), so nothing there is known to relate a mapping to the wall. A
configured value stands, and stands *declared* — `sevr=minor … this value cannot
be verified and is taken as given`. Silence is reserved for the two cases that
have earned it: the target confirmed what was asked, or nobody asked for
anything.

### 11.3 RTEMS: the budget is live, and refuses

Measured on the 256 MB `xilinx-zynq-a9` guest, image built through
`scripts/embedded-image.sh rtems ca`, default budget:

```
WALL attempt=33 held=32
  CAS: no resources for a new client (worker pool at its thread-memory budget:
  this set needs 3584 KiB, 158 of 160 MiB already reserved — raise
  EPICS_RS_POOL_RESERVATION_MB if the target has the memory)
```

The IOC survived and kept serving for the rest of the run: `CA_CONN_CNT=37`,
`FD_CNT=45`, `MEM_USED=86,427,536` — a third of the guest, with the budget
refusing at 158 of 160 MiB reserved. So the memory gate *is* what refuses on
RTEMS, not the count bound and not the heap, and §10.4's arithmetic was wrong
because it compared the budget against the guest's RAM rather than against what
the pool had actually reserved. An RTEMS CA set is 3584 KiB where a VxWorks one
is 5120 KiB, which is why 160 MiB buys 32 sets there and 32 sets here are a
different number of bytes.

The console carried the same refusal with its ordinal
(`… — refused 10.0.2.2:41620 (refusal #1)`), and nothing fatal appeared.

### 11.4 The refusal counter is right; the thread that publishes it is starved

`RTEMS:CA_REFUSED_CNT` reading 0.0 straight after a refusal was written off as a
poll-interval effect. It is not, and the poll interval is not the interesting
part.

Sampled on VxWorks, 1536M guest, 1200 MiB budget, one 136-client ramp:

| probe time | UPTIME | CA_CONN_CNT | CA_REFUSED_CNT |
|---|---|---|---|
| t+0 s | 00:00:00 | 0 | 0 |
| t+12 s | 00:00:12 | 5 | 0 |
| t+413 s (refusal) | 00:00:12 | 5 | 0 |
| t+415 s | **00:06:55** | 141 | 1 |

`UPTIME` changes once a second by construction. It did not move for 401 seconds
— the whole ramp — and then jumped 6 min 43 s in a single tick two seconds after
the load stopped. The counter was correct all along; nothing published it.

It is priority starvation, not lock coupling, and the console settles which. The
`c6-probe` thread shares no lock with the status pusher, sleeps 10 s and prints;
over the same 444 s run it emitted **4** ticks instead of ~44, and its
`FDPROBE seq=1` reports `CA_CONN_CNT=5` while `seq=2` already reports 141. Both
threads froze together, and the only thing they share is their band:
`ThreadPriority::Low` (EPICS 10). On a single-CPU target under `SCHED_FIFO`,
"below the serving threads" means "never, while the server is busy".

This is a deviation from base, not a tuning choice. C's `devIocStats` status
records are updated by the periodic scan tasks at `epicsThreadPriorityScanLow`
(60) / `ScanHigh` (70), both **above** `epicsThreadPriorityCAServerLow` (20) and
`CAServerHigh` (40) — so under C the operator can read a loaded IOC. We put the
only equivalent below the CA server, so ours goes blind exactly when it is
needed. The fix is the band: `status_pv.rs`'s pusher belongs at a scan band, and
the module comment that argues for `Low` ("it reports, it does not serve — so it
must never be the reason a scan or a CA reply waits") is the reasoning that
produced the defect; a 1 Hz walk of five values is not what makes a CA reply
wait.

`crates/epics-base-rs/src/server/status_pv.rs:296` is not this panel's file, so
the one-line band change is handed over rather than made here.

### 11.5 The rule: a diagnostic path must publish while the IOC is at its wall

§11.4 is not "one status PV is stale". Two independent threads went blind for
401 s, and e10-residue saw the same shape from the other side on VxWorks —
accept latency from 0.02 s to 7.36 s past 40 held clients, with the console
stopping — so this is a class, and the fix belongs at the class.

**Invariant.** MUST: every thread on the diagnostic path — the status-PV
publisher and anything else whose only job is to let an operator see the box —
keep running while the IOC is being driven to its admission wall. MUST NOT: a
diagnostic thread sit below the serving threads on a target whose scheduler is
strictly priority-ordered, because there "below" means "never, while the server
is busy".

**Owner.** Applying a band has exactly one owner and it holds:
`runtime::task::enter_ioc_thread` is the sole place a thread takes its EPICS
priority (`task.rs:1094`), every thread body calls it, and `task.rs:3053`'s
source scan fails the build for a thread body that calls neither it nor
`name_current_thread()`. Nothing bypasses it — audited across the workspace:
`worker_pool.rs:1250` (pool workers, from `role.priority`),
`epics-ca-rs/src/server/blocking.rs:443,587`,
`epics-pva-rs/src/server_native/blocking.rs:802,1132`, `task.rs:1705,1825,1904,1995`
(the spawn helpers), and the two bring-up probes.

**The gap.** *Choosing* the band has no owner. The serving paths name theirs and
pin them: `CAS_TCP_PRIORITY` / `CAS_UDP_PRIORITY` (`blocking.rs:184,197`) with a
guard asserting each loop enters at its own constant (`blocking.rs:2015`), and
`PVA_SERVER_PRIORITY` / `PVA_UDP_PRIORITY` likewise. The diagnostic path passes a
bare `ThreadPriority::Low` literal at three call sites — `status_pv.rs:296`,
`realtime-ca-ioc.rs:776`, `realtime-pva-ioc.rs:826` — with no named constant and
no guard. So the band was never chosen against a rule; it was argued locally,
once, in the comment above `status_pv.rs:296`: *"this is the least urgent thread
in the IOC by construction — it reports, it does not serve — so it must never be
the reason a scan or a CA reply waits."* That argument is why the path is at the
bottom, and it is the defect: it optimises for never delaying a reply and pays
with never being published.

**What C does.** C reaches the opposite arrangement and states it in numbers.
`libcom/src/osi/epicsThread.h:77-85`: `Low`=10, `CAServerLow`=20,
`CAServerHigh`=40, `Medium`=50, `ScanLow`=60, `ScanHigh`=70. The whole CA server
lives at the bottom of that: `caservertask.c:109` creates `CAS-client` at
`epicsThreadPriorityCAServerLow`, and `:554-560` puts the TCP listener at
`CAServerLow-2`, the name receiver at `CAServerLow-4`, the beacon sender at
`-3`, the TCP sender at `-1`, while `:1508` places the event task at
`epicsThreadHighestPriorityLevelBelow(CAServerLow)`. Everything that publishes a
record value on a period runs 40 levels above all of it:
`dbScan.c:949` spawns each `scan-%g` at `epicsThreadPriorityScanLow + ind`, and
`dbScan.c:776` puts `scanOnce` at `ScanLow + nPeriodic`. devIocStats' status
records are ordinary periodic records, so under C an operator reads a saturated
IOC because record publication outranks client service by design.

Ours inverts it: the publisher at `Low`(10) is ten levels *below*
`CAServerLow`(20), which our own `map_epics_priority_*` carries faithfully to
the target. Under `SCHED_FIFO` on a single CPU that is the measured 401 s.

**The change.** Not three call sites moved one at a time: a single owner for
*which role sits at which band*, so the band stops being an argument a spawn
site can invent. See §11.6, which is the built and measured version of this
paragraph.

The two bring-up probes were expected to stay at `Low`, on the argument that
`c6-probe` writes an `fd_census` line per descriptor — 146 lines to a serial
console at 141 clients — where the 1 Hz five-value push does not. §11.6 keeps
that conclusion and replaces the argument with a number.

### 11.6 The band owner, and what it costs on each side

`runtime::ioc_role` is the owner §11.5 said was missing. A role names itself
(`IocRole::StatusPublisher`, `IocRole::ConsoleCensus`), `IocRole::band` is the
one table that answers, and `enter_ioc_role` / `spawn_ioc_role` are what a
thread calls. The three files that used to write a band literal are guarded
against writing one at all: each asserts its own production scope contains no
`ThreadPriority` (`status_pv.rs`, `realtime-ca-ioc.rs`, `realtime-pva-ioc.rs`),
so the band cannot be re-argued at a creation point.

The serving bands stay with their servers. They already have named constants
and their own guards (`blocking.rs:184,197,2015` and the PVA pair), and each is
derived from the C line that sets it; the table states the ordering against C's
`CAServerHigh` rather than reaching across crates for theirs.

**Both sides of the rule are measured.** Every run below is the same rig, guest
and probe; VxWorks 7 on qemu, 1536 MB guest, `EPICS_RS_POOL_RESERVATION_MB=1200`,
one serial ramp of CA clients to the pool's 141-client capacity.

| image | probes | held at wall | `UPTIME` across the ramp | `CONN` at the wall |
|---|---|---|---|---|
| pre-fix | on | 136 | frozen `00:00:12` for 420 s | 5 |
| both roles at `ScanLow` | on | **87** | live | 92 |
| both roles at `ScanLow` (rerun) | on | **87** | live | 92 |
| status only at `ScanLow` | off | 136 | live, `00:00:00`→`00:07:08` | 141 |
| status only at `ScanLow` | on | 136 | live, `00:00:01`→`00:07:14` | 141 |

Raising the status publisher costs nothing: 136 admitted either way, and the
refusal is the same `worker pool at capacity: 141`. Raising the console census
with it costs 36% of the ceiling, twice, the 88th client's handshake exceeding
its 15 s timeout while the server keeps accepting — an instrument rewriting its
own measurement. So the table answers per role: `StatusPublisher` at `ScanLow`,
`ConsoleCensus` at `Low`, and the ordering test matches exhaustively so a new
role has to pick a side.

What the operator sees at the wall, which is the point of the exercise: pre-fix
`CONN=5, REFUSED=0.0, UPTIME=00:00:12` while 141 clients are connected and one
has been turned away; after, `CONN=141, REFUSED=1.0` two seconds later, holding
for the 30 s the wall is held.

**RTEMS, same rule, same order.** The two targets do not share a priority map —
RTEMS inverts through `RTEMS_MAXIMUM_PRIORITY` and VxWorks is measured directly
— but both land on POSIX `56 + epics`, so both are strictly increasing in the
EPICS value, and VxWorks's own layer then inverts once more to `vx = 199 -
epics` where smaller is more urgent. All three orderings agree, asserted in
`the_ordering_survives_both_embedded_priority_maps`.

On target: armv7 RTEMS 6 guest, 256 MB, 16 standing clients, then 60 s of
accept churn — connect, handshake, close — which is the load that starves,
refusal churn having been ruled out (a refused client never spawns a set, and
both arms publish through it).

| image | churn cycles in 60 s | `UPTIME` at +10/+20/+30/+40/+50 s |
|---|---|---|
| pre-fix | 6000 | `00:00:22` at all five — frozen 40 s |
| after | 5979 | `00:00:23`, `00:00:33`, `00:00:42`, `00:00:53`, `01:02` |

0.4% fewer accept cycles, and the operator's view stops going dark. The RTEMS
wall itself reads correctly too: refusal at attempt 33 (`this set needs 3584
KiB, 158 of 160 MiB already reserved`), and under sustained refusal churn
`CA_REFUSED_CNT` tracks the client-side count within 2 at every sample —
16/19, 33/35, 50/52, 66/68, 83/85, 100/100 — with `UPTIME` advancing 1 s/s
throughout.

**Still open.** The bring-up console reporter goes quiet under ramp load, by
decision rather than by accident: 4 ticks in 478 s at `Low` against 9 in 116 s
at `ScanLow`. That is the symptom e10-residue raised, and for that path it is
not fixed — the operator's instrument on a shell-less target is the status PVs,
and a periodic unbounded console dump cannot sit above the threads it measures.
Bounding the census's per-tick output would let it move up; that is a change to
what the probe prints, not to which band it takes.
