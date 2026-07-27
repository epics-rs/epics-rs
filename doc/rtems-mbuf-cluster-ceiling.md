# The mbuf-cluster ceiling: a third arena, and crossing it does not produce a refusal

Measured 2026-07-27 on gv100, `xilinx_zynq_a9_qemu`, image `caioc-e10`. Handed
to **refusal-fidelity**. Nothing under `crates/` was touched.

## Where the ceiling is

libbsd does not take packet memory from `RTEMS_Malloc_Heap`. It has its own
allocator domain, and the size is a build-time constant:

```c
/* rtemsbsd/include/rtems/bsd/bsd.h:59 */
#define RTEMS_BSD_ALLOCATOR_DOMAIN_PAGE_MBUF_DEFAULT (8 * 1024 * 1024)
```

```c
/* freebsd/sys/kern/kern_mbuf.c:140,146 */
maxmbufmem = rtems_bsd_get_allocator_domain_size(RTEMS_BSD_ALLOCATOR_DOMAIN_PAGE);
...
if (nmbclusters == 0)
        nmbclusters = maxmbufmem / MCLBYTES / 4;
```

`MCLBYTES` is 2048 (`MCLSHIFT` 11, `freebsd-org/sys/sys/param.h:173`), and
`crates/epics-rtems-boot/csrc/rtems_init.c:264` calls `rtems_bsd_initialize()`
without assigning `rtems_bsd_allocator_domain_page_mbuf_size`, so the default
stands:

> **nmbclusters = 8,388,608 / 2048 / 4 = 1024 clusters = 2 MiB**

That is the entire packet-cluster budget for the target, and
`malloc_free_space()` — the value C base gates CA admission on, and the one this
panel measured tracking the heap wall exactly — cannot see one byte of it.

## Can the guest be asked?

Not on this image. `kern.ipc.nmbclusters` is a `SYSCTL_PROC`
(`kern_mbuf.c:191`), so libbsd would answer `sysctlbyname`, but the RTEMS image
has no iocsh and no shell, and no status PV publishes it. From outside, the only
route to the number is the build-time derivation above. Publishing it next to
`MEM_FREE`/`MEM_BLK` would make it observable; that is a `crates/` change and is
not made here.

## What a client costs, bracketed

Clusters are consumed by inbound full-size TCP segments, not by monitor traffic:
a CA monitor frame is ~48 B, below FreeBSD's `MINCLSIZE`, so it rides in plain
256 B mbufs from `zone_mbuf`. Measured — 96 non-reading subscribers holding
9,600 subscriptions through 300 writes produced **zero**
`nmbclusters limit reached` lines.

The load that reaches the ceiling is clients that keep sending while refusing to
read: the server's writes block its per-client thread, that thread stops
reading, and inbound data piles up in the receive socket buffer. Threshold
search on client count, 25 s of blast each, fresh boot per arm:

| clients | pushed | `nmbclusters limit reached` |
| --- | --- | --- |
| 8 | 27,197,758 B | 0 |
| 16 | 50,918,048 B | 0 |
| 32 | 90,353,824 B | 3 |

Exhaustion at 32 and none at 16 brackets per-client occupancy at

> **32 ≤ clusters/client < 64**, i.e. 64 KB ≤ 128 KB of cluster memory per
> blasting client

against a total of 1024. Roughly 32 hostile clients, or any client population
whose aggregate unread receive backlog reaches 2 MiB, is the whole budget.

## What the IOC does at the ceiling: neither refuse nor recover

This is the part that matters. Below the ceiling the IOC stalls and comes back;
at the ceiling it does not come back. Same blast, load released with `SO_LINGER
0` so the host sends RST and nothing keeps trickling in, then a fresh CA client
probed every 20 s:

| clients | `nmbclusters` lines | `C6 seq=` console lines | outcome |
| --- | --- | --- | --- |
| 8 | 0 | 12 | **RECOVERED 96 s** after release |
| 16 | 0 | 12 | **RECOVERED 128 s** after release |
| 32 | 3 | 0 | **NO RECOVERY within 700 s** |

At 32 the console thread — a 10 s cadence — printed nothing at all for the ~730 s
the arm ran, i.e. about 73 missed passes, and no `panic`, `FATAL` or assertion
text ever appeared.

Console silence is not death, so the guest was measured rather than assumed.
qemu host CPU for the guest process, sampled from `/proc/<pid>/stat`
(`utime + stime`):

| state | CPU |
| --- | --- |
| idle IOC | 43 jiffies / 10 s (≈ 4 %) |
| wedged, +20 s after the blast | 3014 jiffies / 30 s (≈ 100 %) |
| wedged, +2 min later | 3012 jiffies / 30 s (≈ 100 %) |

Process state `S`, `comm` still `qemu-system-arm`. **The guest is not halted —
it is spinning at 100 % CPU, steadily, with its load gone.** That is a livelock,
not a crash and not a refusal.

So the answer to the question this arm was set is: at the mbuf ceiling the IOC
does not refuse. It stops serving CA entirely, stops printing, burns the CPU,
and has not returned 700 s after the last packet. By the standard applied to the
other two walls — the pool cap refuses politely, the heap wall refuses politely
via the `EAGAIN` catch — this third arena has no gate at all.

## What is not established

* **What spins.** Attributing the 100 % CPU to a specific loop (libbsd retrying
  a cluster allocation, or a server read loop turning `ENOBUFS` into a busy
  wait) needs a guest-side profile or an instrumented build. RTEMS has no
  post-mortem facility here equivalent to VxWorks `rtpShow`/`edrShow`, so
  "wedged" versus "dead" is settled only as far as the CPU measurement settles
  it: something is executing.
* **Whether it ever recovers.** 700 s is the observed bound, not a proof of
  permanence.
* **The 8- and 16-client outages.** Those recover, but they still cost 96 s and
  128 s of no service with no mbuf exhaustion at all, so they belong to the
  priority-band family refusal-fidelity is already fixing rather than to this
  ceiling.

## One measurement hazard worth recording

The first version of this arm had every load client subscribe 100 times to the
same PV. `record_instance` caps subscribers per record *field* at 1024
(`record field subscriber cap reached ... live=1024 cap=1024`), and each refused
`EVENT_ADD` past the cap prints two WARN lines to the serial console — 4,346
console lines in one arm. Serial console output is itself a load, so that arm
measured its own logging. The subscription phase is optional in the harness for
this reason (`NSUB=0`).

## Evidence

`doc/vxworks-e10-rig/evidence-e10-heap/` — `mbc8/mbc16/mbc32.log.gz` (threshold
search), `rec8/rec16/rec32.log.gz` (recovery), `rec32.console.log.gz` and
`cpu32.console.log.gz` (guest console at the ceiling), `mbc32.console.log.gz`.
Harness: `doc/vxworks-e10-rig/rtemsmbuf-e10.py`,
`doc/vxworks-e10-rig/rtemsrecover-e10.py`.
