# The CA admission gate is one set wider than the platform tolerates

`epics-ca-rs`'s blocking CA server has one admission point,
`CAS_CLIENT_POOL.acquire()` in `server::blocking`, which borrows a client's two
worker threads together. It fails two ways: `WouldBlock` when the pool is at
`CAS_CLIENT_POOL_CAPACITY = 141`, and the spawn error when the OS refuses a
thread. At 1024M the capacity arm is unreachable — the wall arrives at 58 sets —
so the only refusal that fires is `pthread_create` reporting `EAGAIN`
(`os error 11`), which reaches the peer as
`CA_PROTO_ERROR: CAS: no resources for a new client`. The gate refuses on the
*symptom* of exhaustion rather than against a budget: it tries, and believes the
OS when the try fails.

Neither the branch measured here nor the rig tree it was built from contains a
reservation budget (`rg per_thread_overhead|POOL_RESERVATION` finds nothing
outside `doc/`), so nothing below has been measured against one.

In 5 of 16 measured boots the try did not fail. `pthread_create` succeeded for
one more set than the platform could carry, and that set died — four times as a
page fault inside the newly created task, once as a heap abort on the accept
thread.

## The census

Every boot of `realtime-ca-ioc.vxe` (RTP on `qemu-system-x86_64`) that has been
ramped to its admission wall, across the declared-stack sweep
([sweep doc](vxworks-ca-admission-wall-vs-declared-stack.md)) and the frame-pool
A/B ([A/B doc](vxworks-ca-frame-pool-on-target.md)). `SETS` and `REFUSED` are
from the image's own `POOLPROBE seq=1` line; the fault column names the task in
the console exception dump.

| boot | classes, RAM | served | `SETS` | `REFUSED` | outcome |
|---|---|---:|---:|---:|---|
| `scSmall` | Small/Medium 1024M | 67 | 67 | 4 | clean refusal |
| `scMedium` | Medium/Medium 1024M | 58 | 58 | 4 | clean refusal |
| `scBig` | Big/Medium 1024M | 49 | 49 | 4 | clean refusal |
| `scMedEvBig` | Medium/Big 1024M | 49 | 49 | 4 | clean refusal |
| `noPool2` | Medium/Medium 1024M | 58 | 58 | 4 | clean refusal |
| `nopoolM4` | Medium/Medium 1024M | 58 | 58 | 4 | clean refusal |
| `nopoolM5` | Medium/Medium 1024M | 58 | 58 | 4 | clean refusal |
| `poolMedium2` | Medium/Medium 1024M | 58 | 58 | 4 | clean refusal |
| `poolM3` | Medium/Medium 1024M | 58 | 58 | 4 | clean refusal |
| `poolM4` | Medium/Medium 1024M | 58 | 58 | 4 | clean refusal |
| `poolM5` | Medium/Medium 1024M | 58 | 58 | 4 | clean refusal |
| `nopoolM3` | Medium/Medium 1024M | 58 | **59** | **0** | page fault, `CAS-client 58` |
| `poolMedium` | Medium/Medium 1024M | 58 | **59** | **0** | page fault, `CAS-client 58` |
| `scSmallEvSmall` | Small/Small 1024M | 80 | **81** | **0** | page fault, `CAS-client 80` |
| `scSmall1152` | Small/Medium 1152M | 105 | **106** | **0** | page fault, `CAS-client 105` |
| `scBigEvSmall` | Big/Small 1024M | 53 | — | — | heap abort, signal 6 |

Three invariants hold without exception across the 16:

1. The two outcomes are mutually exclusive. Every boot that refused has
   `REFUSED=4` and `SETS` equal to the served count; every boot that faulted has
   `REFUSED=0` and `SETS` equal to served **+ 1**. The gate either refuses or
   admits exactly one set too many — never both, never two too many.
2. The faulting task is always `CAS-client (SETS − 1)`: the client thread of the
   last-admitted set, in 0-based roster naming.
3. The served count is the same whether the boot refused or faulted. At
   Medium/Medium 1024M it is 58 in all ten boots, including the two that
   faulted. The fault costs the 59th client, not any of the 58.

Crossing is not confined to one configuration or to one build: it occurs at
three stack-class combinations and two guest RAM sizes, with and without
`8375ca36`'s frame pool (once in five pooled boots, once in five un-pooled boots
at the same configuration).

## The fault, verbatim

`doc/vx-rig-e8/logs-pool-ab/evidence-poolfault.txt` and
`evidence-nopoolfault.txt` — the guest console output as captured, with the
serial line's trailing CR removed by this repo's text normalization and nothing
else changed. The 59th client's task gets as far as applying its EPICS priority
and then faults:

```
PRIOPROBE label=CAS-client 58 epics=20 applied=Realtime getsched_rc=0 policy=1 posix=76 taskprio_rc=0 vx=179 expect_posix=76 expect_vx=179

Page Fault Num=0xe

Esp0 0xffff800011187000 : 0x0000000000000001, 0xffffffff9ee9dbf0, 0xffffffffffffff20, 0xeeeeeeeeeeeeeeee
Esp0 0xffff800011187020 : 0xffff800011187040, 0xffffffff80d48e71, 0x00000000000000e0, 0xeeeeeeeeeeeeeeee

Page Dir Base   : 0x000000002a37c000                Program Counter : 0xffffffff80d48dfd
Code Selector   : 0x0000000000000008                Eflags Register : 0x0000000000000246
Error Code      : 0x0000000000000000
Page Fault Addr : 0x0000000000000490 

Task: 0xffff8000103efa00 "CAS-client 58"
0xffff8000103efa00 (CAS-client 58): RTP 0xffff800008c40000 has had a failure and has been deleted.
```

Per faulting boot: two faults in the `CAS-client` task and a third in
`"cbTimer"`. Every fault in all four boots is at
`Page Fault Addr 0x0000000000000490` with `Program Counter 0xffffffff80d48dfd`,
a kernel-text address — the same kernel routine faulting in whichever task runs
next, not a Rust-level dereference.

The last set's two threads do not fail together. `CAS-client 58` printed the
`PRIOPROBE` line above and then faulted; `CAS-event 58` printed no `PRIOPROBE`
at all in either boot, while `WORKERS=118` counts both of the set's threads as
spawned. So the roster admitted and counted a set whose event thread never
reached its priority application.

Blast radius, stated as measured:

- The console says `RTP ... has had a failure and has been deleted`, but the RTP
  kept running: `POOLPROBE`/`FDPROBE` continue to `seq=17` in both A/B faulting
  boots, and in `poolMedium` a CA read of `RTEMS:CA_CONN_CNT` from the host
  succeeded *after* the fault, returning `58.0`. The IOC survives.
- The last-admitted client does not. It is accepted, counted in
  `SETS`/`WORKERS`, given no reply, and released only by its own 20 s
  client-side timeout:
  `FAIL attempt=54 held=53 total=58 elapsed=20.02s OTHER(TimeoutError: timed out)`.
  From the peer's side that is indistinguishable from a hung server.
- What faulted at `0x490` is **not identified here**. `0x490` is a small offset
  from a null base and the PC is in kernel text, which is consistent with a
  kernel object that failed to allocate being dereferenced unchecked, but no
  RTP-visible query was run to confirm it and no such query is known to track
  this ceiling.
- `scBigEvSmall` shows the same crossing with a different terminal symptom —
  the heap ran out first and Rust aborted on the accept thread
  (`memory allocation of 81 bytes failed`, `deleted due to signal 6`) before any
  probe ran. There the IOC did **not** survive.

## Why the gate cannot see it

`pthread_create` returning success is not evidence that the task can run. The
reserved-address-space ceiling this platform enforces (a thread costs its
declared stack plus roughly 1 MiB) is charged at thread creation; whatever fails
at `0x490` fails afterwards, inside the new task. A gate whose only signal is
`pthread_create`'s return value is therefore structurally one set too wide, and
which side of the boundary a boot lands on is not deterministic — the same
binary at the same guest RAM refused cleanly in four boots and crossed in the
fifth.

Two consequences for the refusal path:

- `REFUSED` is 0 in exactly the boots where a client was harmed. The counter
  reports gate activations, so the damaging case is the one it reports as quiet.
  Nothing in the server's own telemetry distinguishes "wall not reached" from
  "wall crossed and a client is dead".
- A budget gate — admit only while a reservation for the set's declared stack
  plus measured per-set overhead remains — refuses *before* `pthread_create`,
  and refuses deterministically. The overhead such a budget needs is already
  measured: 1,220,973 B per thread on top of the declared stack against a
  271,038,782 B budget, R² = 0.9872
  ([sweep doc](vxworks-ca-admission-wall-vs-declared-stack.md)).

## What is not established

- **A rate.** 5 of 16 boots overall, 2 of 10 at Medium/Medium 1024M. Sixteen
  boots do not fix a probability, and the sweep configurations have one boot
  each.
- **The faulting allocation.** Named above as unidentified rather than guessed.
- **Whether a budget gate closes it.** The budget parameters are measured; no
  build with a budget-based gate has been booted against this ramp.
