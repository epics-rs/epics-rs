# epics-base: `RTEMS-posix/osdEvent.c` leaks one `epicsEventOSD` per event lifecycle

Evidence package for an upstream report. Report prose is deliberately not
written here (standing rule: upstream prose is hand-written); this file is the
complete, independently re-verified evidence it will be written from.

Every claim below was re-verified on **2026-07-23** — none is quoted solely
from the audit agent's report. The Sourcing table at the bottom maps each
claim to how it was verified.

## The defect

`modules/libcom/src/osi/os/RTEMS-posix/osdEvent.c` — `epicsEventCreate`
allocates the wrapper on the heap; `epicsEventDestroy` destroys the semaphore
inside it but never frees the wrapper:

```c
/* :24-26 */
typedef struct epicsEventOSD {
    rtems_binary_semaphore rbs;
} epicsEventOSD;

/* :31-42 */
LIBCOM_API epicsEventId
epicsEventCreate(epicsEventInitialState initialState)
{
    epicsEventOSD *pSem = malloc (sizeof(*pSem));      /* :34 */

    if (pSem) {
        rtems_binary_semaphore_init(&pSem->rbs, NULL);
        if (initialState)
            rtems_binary_semaphore_post(&pSem->rbs);
    }
    return pSem;
}

/* :44-48 — no free(pSem) */
LIBCOM_API void
epicsEventDestroy(epicsEventId pSem)
{
    rtems_binary_semaphore_destroy(&pSem->rbs);
}
```

`rtems_binary_semaphore_destroy` tears down the semaphore in caller-provided
storage; it does not free the enclosing allocation. Every
create/destroy cycle of an `epicsEvent` on this port leaks
`malloc(sizeof(epicsEventOSD))` permanently.

## Which builds are affected

- `configure/toolchain.c:31-36` — `__RTEMS_MAJOR__ >= 5 ⟹ OS_API = posix`.
  So **every RTEMS ≥5 build** (including RTEMS 6) compiles this file;
  the unaffected `RTEMS-score/osdEvent.c` is used only on RTEMS ≤4.
- `configure/CONFIG_COMMON:140-142` vpath order
  (`. ../os/RTEMS-posix ../os/RTEMS ../os/posix ../os/default`) makes
  `RTEMS-posix/osdEvent.c` override `posix/osdEvent.c`.

## Still present upstream, never reported

- Upstream `master` fetched 2026-07-23 from
  `https://raw.githubusercontent.com/epics-base/epics-base/master/modules/libcom/src/osi/os/RTEMS-posix/osdEvent.c`
  — `epicsEventDestroy` is byte-for-byte the version above, no `free()`.
- The file was introduced by **PR #206** "Added Heinz's new osdEvent.c to
  RTEMS-posix" (merged 2022-01-24, merge commit `1655d68e`, fixes issue
  #202) — the leak has been present since introduction.
- No existing report found: `gh search issues` for `epicsEventDestroy` (one
  hit, #445, an unrelated Linux `osdMutex` test race), for `osdEvent leak`
  (zero hits); `gh search prs` for `osdEvent` (three hits: #253, #206, #131 —
  none address the free).

## Evidence that it is an omission, not house style

Three sibling implementations all release their storage:

| site | behavior |
|---|---|
| `os/posix/osdEvent.c:79` (and error path `:67`) | `free(pevent)` ✅ |
| `os/RTEMS-score/osdEvent.c:66-75` | classic-API `rtems_semaphore_delete`; no heap wrapper exists ✅ |
| `os/RTEMS-posix/osdMessageQueue.c:74` | same directory, same author-era: `free(id)` ✅ |

## Reach: what an RTEMS ≥5 IOC leaks through this

**Every thread lifecycle.** RTEMS ≥5 has no `osdThread.c` override, so it
builds `os/posix/osdThread.c`; every `epicsThreadOSD` owns a `suspendEvent`
created at `:179` and destroyed (leaking its wrapper) in `free_threadInfo` at
`:235`. One block per epicsThread create/exit cycle, unconditionally.

**Per CA-server (rsrv) client connect/disconnect cycle — ≥5 blocks:**

| # | event | create | destroy |
|---|---|---|---|
| 1 | `CAS-client` thread `suspendEvent` | `osdThread.c:179` | `:235` |
| 2 | `CAS-event` thread `suspendEvent` | `osdThread.c:179` | `:235` |
| 3 | `client->blockSem` | `caservertask.c:1262` | `:1128` |
| 4 | `evUser->ppendsem` | `dbEvent.c:314` | `:396` |
| 5 | `evUser->pexitsem` | `dbEvent.c:320` | `:395` |
| +1 per cancelled monitor | `db_sync_event` `wait.wake` | `dbEvent.c:572` | `:591` |

**Per libca client virtual circuit — 6 blocks, including failed connect
attempts.** `tcpiiu` constructs `recvThread` and `sendThread` as joinable C++
`epicsThread` objects before the connect outcome is known
(`tcpiiu.cpp:676-682`, `cac.cpp:554-559`); each C++ `epicsThread` carries
`epicsEvent event; epicsEvent exitEvent;` members (`epicsThread.h:440-441`)
plus its OS-thread `suspendEvent` — 3 wrapper blocks per thread lifecycle,
2 threads per circuit.

Unbounded in connect/disconnect count: a cycling client (or a flapping
network link driving libca reconnects) grows the heap monotonically.

## What a correct fix looks like

Add the missing release to `epicsEventDestroy`:

```c
LIBCOM_API void
epicsEventDestroy(epicsEventId pSem)
{
    rtems_binary_semaphore_destroy(&pSem->rbs);
    free(pSem);
}
```

One line; mirrors `os/posix/osdEvent.c:79` and the same directory's
`osdMessageQueue.c:74`.

## Not measured / open

- **Bytes per leaked block**: `sizeof(epicsEventOSD)` =
  `sizeof(rtems_binary_semaphore)` + RTEMS heap-block overhead. No RTEMS
  headers on the audit machine; needs a target measurement (our bring-up box
  has the toolchain).
- `os/RTEMS-posix/osdThreadExtra.c:46-47` runs `pthread_setname_np` on every
  thread create (default hook); whether RTEMS's implementation allocates is
  unchecked.
- No on-target repro was run for the C IOC (would need a C IOC image cycling
  clients while logging heap; our rigs currently boot the Rust IOC).

## Relation to our own finding

Our Rust port leaks 176–179 B per `std::thread` creation on RTEMS for a
*different* root cause (TLS key freed before its destructor runs;
upstream-libc issue, measured on target 2026-07-22). This C defect means the
prior comparison claim "C leaks 0 per thread" holds only for raw pthreads and
the Linux port — an RTEMS ≥5 **C** IOC also leaks per thread cycle, via a
missing `free`. The two defects are independent and both upstream.

## Sourcing

| claim | verified how |
|---|---|
| `osdEvent.c` code, `:34` malloc / `:44-48` no free | direct Read of `/home/stevek/work/epics-base/.../RTEMS-posix/osdEvent.c` (2026-07-23) |
| local tree identity | `git log -1` → `669a25697` (local branch `oracle-ground-truth-fixed`; carries local calc fixes, none touching libcom/osi — upstream check below is the authority) |
| still on upstream master | WebFetch of raw.githubusercontent.com master file (2026-07-23) |
| introduced by PR #206, merged 2022-01-24, `1655d68e` | `gh pr view 206` |
| no existing issue/PR | `gh search issues` / `gh search prs` (queries listed above) |
| `toolchain.c` OS_API split | rg of `configure/toolchain.c:33/:35` |
| `osdThread.c:175/:179/:235` create/destroy pairing | sed slice of the file |
| rsrv/dbEvent table rows 3–5, `:572/:591` | sed slices of `caservertask.c`, `dbEvent.c` |
| `epicsThread.h:440-441` C++ members | sed slice |
| contrast sites (posix `:79`, score, msgQueue `:74`) | rg + sed slices |
| `tcpiiu` two-joinable-threads-per-circuit | sed slices: `tcpiiu.cpp:676-682` ctor member-init builds `recvThread`/`sendThread` before connect outcome; `cac.cpp:556-559` placement-new in `freeListVirtualCircuit`; `epicsThread.cpp:212-216` C++ wrapper sets `opts.joinable = 1` and creates the OS thread in the constructor |
| freelists are bounded caching, not leaks | sed slices of `freeListLib.c` — `freeListMalloc` mallocs `nmalloc`-item blocks (`:121`), `freeListFree` recycles (`:156`), blocks released only by `freeListCleanup` (`:177`) |

(The full audit narrative is the c-base-audit panel report, round
`01KY7KGHXTR0FCDWP3ZVPYWNAA`; every citation used here was re-verified
directly as listed above.)
