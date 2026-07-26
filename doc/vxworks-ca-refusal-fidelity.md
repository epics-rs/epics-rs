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

* **The wire status still collapses the two gates.** `available` = 48 for both,
  because every alternative that is semantically right is `CA_K_ERROR` and
  aborts default-handler clients (§5). Closing this needs a new CA status code
  with `CA_K_WARNING` severity — a wire-visible change, not taken here.
* **`RTEMS:CA_REFUSED_CNT` reads 0 while the wall is loaded.** The status PVs
  starve under the ramp (a known band-189 effect on this target), so the
  server-side refusal counter is not a usable cross-check at the wall; the
  numbers above are console and wire counts.
* **The 1024M allocator abort** (§6.3) is measured and not root-caused. Out of
  this family, and pre-existing on `origin/main`.
