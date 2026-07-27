# Why the 768M and 832M VxWorks arms produce no IOC

Both arms look identical from outside — `rtpSp` returns a valid RTP id and the
ramp then finds nothing listening — and both were carried as unexplained. They
are not the same failure, and only one of them is silent. Measured 2026-07-27
on gv100, image `ca-e11.vxe` (35,736,400 bytes), one guest at a time,
`doc/vxworks-e10-rig/{nostart,bandladder,poolladder}-e10.sh`.

## The ladder

`bandladder-e10.sh` boots one guest per RAM, waits past the `rtpSp`, and then
asks the **kernel** — `rtpShow`, `rtpMemShow`, `edrShow` — because the console
is not a witness for a failure that happens before or instead of `main`.

| `-m` | `OS Memory Size` | outcome | ED&R injection point |
| --- | --- | --- | --- |
| 768M | ~702 MB | segments never load | `rtpLib.c:2847` |
| 800M | ~734 MB | RTP runs, aborts | `rtpSigLib.c:7468` |
| 816M | ~750 MB | RTP runs, aborts | `rtpSigLib.c:7468` |
| 832M | ~766 MB | RTP runs, aborts | `rtpSigLib.c:7468` |
| 848M | ~782 MB | RTP runs, aborts | `rtpSigLib.c:7468` |
| 864M | ~798 MB | RTP runs, aborts | `rtpSigLib.c:7468` |
| 880M | ~814 MB | running, 19 tasks | — |
| 896M | ~830 MB | running, 19 tasks | — |

## 768M — `main` never runs, and the silence is correct

```
Severity/Facility:   FATAL/RTP
Task:                "iCa-e11" (0xffff80000f456400)
RTP:                 "/host.host/ca-e11.vxe" (0xffff80000f44c000)
RTP Address Space:   0x200000 -> 0xb92800
Injection Point:     rtpLib.c:2847

RTP failed loading its segments (errno = 0xb4000f). Abort.
```

`rtpSp` returns `0xffff80000f44c000` because the RTP object and its initial
task `iCa-e11` are created *before* the loader maps the program's segments. The
load then fails, the RTP is destroyed, and a moment later `rtpShow` lists
nothing and `rtpMemShow` answers `RTP not in system`. Nothing reaches the
console because there is no program yet to print: the loader's only report
channel is ED&R.

So for this arm "the IOC could not print its first line" is not a defect to
chase — the IOC did not exist.

## 800M–864M — the RTP runs, and it prints exactly why it is aborting

The premise that these arms print nothing is wrong. At 832M the console
carries:

```
FATAL: the IOC could not create its mandatory `scanOnce` thread: resource
unavailable try again (os error 11). Continuing would leave this IOC answering
clients while the work that thread owns never runs, so the process is aborting
instead (C dbScan.c:943-959 wedges iocInit for the same reason).
0xffff80000f456400 (iCa-e11): RTP 0xffff80000f44c000 has been deleted due to signal 6.
```

`pthread_create` returns `EAGAIN`, and the IOC's own guard turns that into a
deliberate abort rather than a half-built IOC. ED&R agrees: `rtpSigLib.c:7468`,
signal 6, traceback `abort` → `killSc` → `rtpKill`.

At 864M the same failure lands 16 MB further along the startup sequence — past
`iocInit`, past the CA server bind:

```
iocInit: 1 non-local DB link(s) made external
iocInit: 4 external CP link subscriptions (3 PVs warmed)
iocInit: 1 external link opens staged
realtime-ca-ioc: serving 17 records on CA port 5064 (TCP + UDP search), ...
FATAL: the IOC could not create its mandatory `scan-0.2` thread: resource
unavailable try again (os error 11). ...
0xffff80000f608200 (scan-owner): RTP 0xffff80000f44c000 has been deleted due to signal 6.
```

Which thread dies is a function of how far 16 MB of extra guest RAM carries the
startup: `scanOnce` at 800–848M, `scan-0.2` at 864M, none at 880M. That is the
same per-thread reserved-address-space cost the admission wall is made of
(`doc/vxworks-ca-admission-memory-on-target-measurement.md`) — here it binds on
the IOC's own fixed thread set instead of on client threads, so the process
never reaches the point where a client could be refused.

The original 832M arm looked silent because that round's console was overwritten
by the next arm before it was filed; `run-arm-e11.sh` was fixed mid-round to
file a NOT-READY console before tearing down, and this ladder re-measures it
from scratch.

## What is not the cause

The kernel's own heap does not move with `-m` at all. `poolladder-e10.sh`
boots with `VXNOLAUNCH=1` — kernel shell, no RTP — and `memShow` is
byte-identical at 768M, 832M, 896M and 1024M:

```
 free           31686512         10        3168651       31672512
 alloc           1867200        443           4214              -
```

So "the kernel had no room for the 34 MB image" is refuted: 768M and 1024M
offer the loader the same 31,672,512-byte largest free block, and only one of
them fails. What scales with `-m` is the RTP address space, which is where both
the segment load and every subsequent `pthread_create` are actually paid for.

## Operational consequence

The image needs ≥880M of guest RAM (`OS Memory Size` ≥ ~814 MB) to reach
`iocInit`. Below that the arms are not degraded IOCs, they are absent ones, and
the two bands fail differently enough that a bring-up script must distinguish
them: below ~702 MB of OS memory there is no console evidence at all and ED&R
is the only place to look.
