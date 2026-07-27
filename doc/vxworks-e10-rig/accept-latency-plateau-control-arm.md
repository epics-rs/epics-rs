# The accept-latency plateau is not the rig's instrumentation

Handed to **refusal-fidelity**, who owns the priority-band defect this belongs
to. Measured 2026-07-27 on gv100. This file records only the control arm; it
does not propose a fix and nothing under `crates/` was touched.

## Why this arm exists

Every E11 arm that showed the plateau carried the heap-accounting shim and a
mutation dropping the probe cadence to 1 s with a full live-block table dump
per pass. The two E11 control arms (shim-free `ca-ctl`) walled at held=34 and
held=81 — the second one connection short of the knee — so no shim-free arm had
ever been driven past it. Until that gap was closed, "the console dump is
starving the guest" was a live explanation for the whole effect.

`ca-ctl` at 2048M, ceiling 140, pace 1 s, split-timing probe
(`rampprobe-e12.py`, `run-arm-e12.sh`) closes it.

## Result: the plateau reproduces on the shim-free build

| held | `total` | `connect` | `ver` |
| --- | --- | --- | --- |
| 1 … 87 | 0.03–0.08 s | 0.00 s | 0.02 s |
| 88 | 6.39 s | 0.00 s | 6.38 s |
| 89 … 140 | 7.26–7.55 s | 0.00 s | 7.24–7.53 s |

55 connections at held ≥ 86: mean `total` 7.028 s, mean `connect` 0.0000 s,
mean `ver` 7.015 s. The knee is at attempt 88 on the control build against
attempt 83 on the instrumented one, so the shim costs about five connections of
headroom — the same order as the −1..+2 it cost the wall in round 3 — and
nothing else.

**So the plateau is production behaviour, not a rig artefact.**

## Where the 7 s sits, and what this arm cannot say

The probe times four points separately. The whole cost is between the TCP
connect returning and the server's first byte: `ver − connect` ≈ 7.0 s, while
`chan − ver` and `read − chan` stay at 0.01 s each right through the plateau.
Once the server speaks it answers everything at full speed; there is no
per-request slowness anywhere.

What this arm **cannot** separate: qemu SLIRP `hostfwd` accepts the host-side
connection on qemu's own listener, so `connect=0.00` says nothing about when
the guest accepted. The 7.0 s therefore still covers SLIRP→guest SYN, the
guest's accept, the `CAS-TCP` handoff and the worker's VERSION reply as one
span. It is flat to ±0.3 s across 55 connections and three guest RAMs, which is
the signature of a fixed retry constant rather than of queueing, but naming
which constant would need a capture inside the guest.

## The same freeze, on the same shim-free build, in the publishers

The control arm's own console is a second instance of what refusal-fidelity
measured. The `c6-probe` thread has a 10 s cadence and the ramp ran 473.5 s, so
about 47 passes were due. It printed **nine**, and the gap is one tick wide:

```
FDPROBE seq=7 FD_CNT=83 FD_MAX=1000 CA_CONN_CNT=78
FDPROBE seq=8 FD_CNT=5  FD_MAX=1000 CA_CONN_CNT=0
```

`seq=7` lands at 78 connections, i.e. ~78 s into the ramp; `seq=8` lands after
the release at 473.5 s. One 10-second tick spans ~395 s of wall clock, and the
thread resumes as soon as the load stops. `c6-probe` runs at
`ThreadPriority::Low` — EPICS 10, the band `status-pv` and `scan-owner` also
sit in — while every per-client worker is at `CaServerLow` = 20.

The same ordering is present on the other target. `TASKDUMP` from the armv7
RTEMS guest (`doc/rtems-cas-stack-highwater-measurement.md`), POSIX priorities
under the `posix = 56 + epics` map:

| thread | posix | EPICS |
| --- | --- | --- |
| `CAS-client` | 76 | 20 |
| `CAS-event` | 75 | 19 |
| `CAS-TCP` | 74 | 18 |
| `CAS-UDP` | 72 | 16 |
| `status-pv`, `c6-probe`, `scan-owner` | 66 | 10 |

The CA ladder itself is C parity — `blocking.rs` derives `CAS_TCP_PRIORITY` as
`CaServerLow − 2` from `caservertask.c:716` and says so. The publisher band is
not: C's periodic publishers run at `ScanLow + n` (`dbScan.c`), which is
*above* client service, not 10 levels below it.

## Evidence

`doc/vxworks-e10-rig/evidence-e12/arm-ctl2048split.{probe,console}.log.gz`.
