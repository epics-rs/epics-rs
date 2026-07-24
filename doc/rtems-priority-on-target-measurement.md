# RTEMS thread priority — measured on target

Closes §8.0 gap 2 of `doc/rtems-scope-b-session-handoff.md`: *"thread priority
observed on target — unmeasured"*. Everything below is a reading taken from a
booted RTEMS 6 guest, not a claim read out of the source.

## What was measured, and with what

**Method point.** The priority is read **back from the kernel** with
`pthread_getschedparam` on the thread itself, and independently a second time
through a full RTEMS task listing (`rtems_task_iterate` →
`rtems_task_get_priority`). `enter_ioc_thread`'s return value —
`PriorityApplied::Realtime` — was deliberately *not* used as the measurement:
it only says `pthread_setschedparam` returned 0, and a return code and an
effective priority are different claims.

| | |
|---|---|
| box | `coding-agent@192.168.2.128`, qemu-system-arm 8.2.2 |
| guest | `-M xilinx-zynq-a9 -m 256M`, `-nic user,model=cadence_gem` |
| image | `rtems-ca-ioc` from `scope-b-priority` @ `e89599431`, plus the temporary probe below |
| toolchain | `arm-rtems6-gcc 13.3.0`, BSP `xilinx_zynq_a9_qemu`, `RTEMS_BSP_PREFIX=~/rtems-bringup/tools` |
| libc | **`0.2.188` from the path patch `/home/coding-agent/rtems-bringup/libc-bringup`, branch `bringup`, rev `6f64e70d6a2a03b989380aaa719207236343f093`, clean tree** — carries the widened `time_t` and `sockaddr_in::sin_len`; confirmed the resolved source with `cargo metadata --filter-platform armv7-rtems-eabihf` |
| clients | two `camonitor` held for the whole reading (`RTEMS:AO`, `RTEMS:LO`); `RTEMS:CA_CONN_CNT` read back `2` |
| ports | TCP 5064, UDP host 15076 → guest 5064 |

The console readback (`eprintln!`, since `tracing` is what the handoff records
as discarded — and this build does install a console subscriber, but the probe
must not depend on that) and the task-listing dump are temporary scaffolding;
the exact patch is in `doc/rtems-priority-probe.patch`. It is **not** merged:
one of its lines trips `only_the_prologue_reaches_the_banding_call`, which pins
the literal source shape `apply_to_current_thread(priority)` inside
`enter_ioc_thread`.

## Result — the mechanism fires and the map is exact

`policy=1` is `SCHED_FIFO`. `core = 255 - posix`; **lower core is more urgent.**

```
PRIOPROBE label=main kname= epics=-1 applied=not-banded-by-design getsched_rc=0 policy=1 posix=1 core=254
PRIOPROBE label=cbLow kname=cbLow epics=59 applied=Realtime getsched_rc=0 policy=1 posix=115 core=140
PRIOPROBE label=cbMedium kname=cbMedium epics=64 applied=Realtime getsched_rc=0 policy=1 posix=120 core=135
PRIOPROBE label=cbHigh kname=cbHigh epics=71 applied=Realtime getsched_rc=0 policy=1 posix=127 core=128
PRIOPROBE label=cbTimer kname=cbTimer epics=70 applied=Realtime getsched_rc=0 policy=1 posix=126 core=129
PRIOPROBE label=scanOnce kname=scanOnce epics=60 applied=Realtime getsched_rc=0 policy=1 posix=116 core=139
PRIOPROBE label=status-pv kname=status-pv epics=10 applied=Realtime getsched_rc=0 policy=1 posix=66 core=189
PRIOPROBE label=CAS-TCP kname=CAS-TCP epics=18 applied=Realtime getsched_rc=0 policy=1 posix=74 core=181
PRIOPROBE label=CAS-UDP kname=CAS-UDP epics=16 applied=Realtime getsched_rc=0 policy=1 posix=72 core=183
PRIOPROBE label=CAS-client-blocking 10.0.2.2:57688 kname=CAS-client-bloc epics=20 applied=Realtime getsched_rc=0 policy=1 posix=76 core=179
PRIOPROBE label=CAS-event-blocking 10.0.2.2:57688 kname=CAS-event-block epics=19 applied=Realtime getsched_rc=0 policy=1 posix=75 core=180
PRIOPROBE label=CAS-client-blocking 10.0.2.2:57700 kname=CAS-client-bloc epics=20 applied=Realtime getsched_rc=0 policy=1 posix=76 core=179
PRIOPROBE label=CAS-event-blocking 10.0.2.2:57700 kname=CAS-event-block epics=19 applied=Realtime getsched_rc=0 policy=1 posix=75 core=180
```

Against §8.0's expectation, every value matches:

| thread | expected posix | measured posix | |
|---|---|---|---|
| `CAS-client-blocking` (×2 clients) | 76 | 76 | ✅ |
| `CAS-event-blocking` (×2 clients) | 75 | 75 | ✅ |
| `CAS-TCP` | 74 | 74 | ✅ |
| `CAS-UDP` | 72 | 72 | ✅ |
| `scanOnce` | 116 | 116 | ✅ |
| `cbTimer` | 126 | 126 | ✅ |
| `status-pv` | 66 | 66 | ✅ |
| `main` | 1 (core 254) | 1 (core 254) | ✅ |

Also measured, not in §8.0's list but on the same boot: `cbLow` 115, `cbMedium`
120, `cbHigh` 127 — `56 + epics` for 59/64/71.

**Which of the two failure modes §8.0 names applies: neither.** The threads are
*not* all equal at the baseline (they span posix 66…127 against `main`'s 1), so
the policy gate and the `pthread_setschedparam` arm are firing. And the
absolute values are not merely correctly *spread* — they are the intended
`posix = 56 + epics` on every single thread, so the map is not off either.

## The full task listing — where libbsd lands

Never taken before; §8.0 asks for it explicitly. The target has no shell and no
`rt task`, so the listing is produced from inside the image. `obj=` is
`rtems_object_get_name`, `thread=` is `_Thread_Get_name` (see the naming note
below).

```
TASKDUMP begin tag=t4 count=30 scheduler_sc=0
TASKDUMP id=0x09010001 core=255 posix=   0 sc=0 obj=IDLE   thread=IDLE
TASKDUMP id=0x0a010001 core= 98 posix= 157 sc=0 obj=TIME   thread=TIME
TASKDUMP id=0x0a010002 core= 96 posix= 159 sc=0 obj=IRQS   thread=IRQS
TASKDUMP id=0x0a010003 core=100 posix= 155 sc=0 obj=_BSD   thread=swi6: Giant tas
TASKDUMP id=0x0a010004 core=100 posix= 155 sc=0 obj=_BSD   thread=kqueue_ctx task
TASKDUMP id=0x0a010005 core=100 posix= 155 sc=0 obj=_BSD   thread=config_0
TASKDUMP id=0x0a010006 core=100 posix= 155 sc=0 obj=_BSD   thread=swi6: task queu
TASKDUMP id=0x0a010007 core=100 posix= 155 sc=0 obj=_BSD   thread=swi5: fast task
TASKDUMP id=0x0a010008 core=100 posix= 155 sc=0 obj=_BSD   thread=thread taskq
TASKDUMP id=0x0a010009 core=100 posix= 155 sc=0 obj=_BSD   thread=swi1: netisr 0
TASKDUMP id=0x0a01000a core=100 posix= 155 sc=0 obj=_BSD   thread=bufdaemon
TASKDUMP id=0x0a01000b core=100 posix= 155 sc=0 obj=_BSD   thread=vnlru
TASKDUMP id=0x0a01000c core=100 posix= 155 sc=0 obj=_BSD   thread=syncer
TASKDUMP id=0x0a01000d core=100 posix= 155 sc=0 obj=_BSD   thread=softirq_0
TASKDUMP id=0x0a01000e core=100 posix= 155 sc=0 obj=_BSD   thread=bufspacedaemon-
TASKDUMP id=0x0a01000f core=254 posix=   1 sc=0 obj=DHCP   thread=DHCP
TASKDUMP id=0x0b010001 core=254 posix=   1 sc=0 obj=       thread=<empty>
TASKDUMP id=0x0b010002 core=140 posix= 115 sc=0 obj=       thread=cbLow
TASKDUMP id=0x0b010003 core=135 posix= 120 sc=0 obj=       thread=cbMedium
TASKDUMP id=0x0b010004 core=128 posix= 127 sc=0 obj=       thread=cbHigh
TASKDUMP id=0x0b010005 core=129 posix= 126 sc=0 obj=       thread=cbTimer
TASKDUMP id=0x0b010006 core=139 posix= 116 sc=0 obj=       thread=scanOnce
TASKDUMP id=0x0b010007 core=189 posix=  66 sc=0 obj=       thread=status-pv
TASKDUMP id=0x0b010008 core=181 posix=  74 sc=0 obj=       thread=CAS-TCP
TASKDUMP id=0x0b010009 core=183 posix=  72 sc=0 obj=       thread=CAS-UDP
TASKDUMP id=0x0b01000a core=254 posix=   1 sc=0 obj=       thread=<empty>
TASKDUMP id=0x0b01000b core=179 posix=  76 sc=0 obj=       thread=CAS-client-bloc
TASKDUMP id=0x0b01000c core=180 posix=  75 sc=0 obj=       thread=CAS-event-block
TASKDUMP id=0x0b01000d core=179 posix=  76 sc=0 obj=       thread=CAS-client-bloc
TASKDUMP id=0x0b01000e core=180 posix=  75 sc=0 obj=       thread=CAS-event-block
TASKDUMP end tag=t4
```

**`CAS-TCP` does not outrank the network stack.** Measured bands, most urgent
first:

| | core | equivalent posix |
|---|---|---|
| libbsd `IRQS` | **96** | 159 |
| libbsd `TIME` | **98** | 157 |
| libbsd network/daemon threads (12 of them, `swi1: netisr 0`, `swi5: fast taskq`, `swi6: task queue`, `swi6: Giant taskq`, `thread taskq`, `kqueue_ctx taskq`, `softirq_0`, `config_0`, `bufdaemon`, `bufspacedaemon-`, `vnlru`, `syncer`) | **100** | 155 |
| our most urgent live thread, `cbHigh` | 128 | 127 |
| `CAS-client` | 179 | 76 |
| `CAS-TCP` | **181** | 74 |
| `main`, `DHCP` | 254 | 1 |
| `IDLE` | 255 | 0 |

`CAS-TCP` at core 181 sits **81 levels below** libbsd's default band and 85
below `IRQS`. The hazard §8.0 names — a `CAS-TCP` above the network stack — is
absent. The whole live EPICS set (cbHigh at core 128 is the most urgent thread
we create) stays clear of core 100 by 28 levels.

The posix column for the classic tasks is a comparability conversion
(`255 - core`), not a value anything set: libbsd's threads are RTEMS classic
tasks, not POSIX threads.

**One boundary the source comment overstates.** `map_epics_priority_rtems`'s
doc says the image is "collision-free with libbsd by construction". Measured,
libbsd's default band is core **100** and the map's most urgent reachable value
is also core **100** (EPICS 99 → posix 155). That is a *tie*, not a strict
separation — and `rtems_priority_map_stays_below_the_libbsd_network_band`
asserts exactly `core >= 100`, so the test is right and the prose is loose. No
live thread is affected: the highest EPICS priority any of our threads takes is
71 (`cbHigh`). The test's comment also says "eleven of its threads sit here";
measured on this boot it is **twelve**.

## The naming half works — the classic listing is the wrong instrument

First dump showed `rtems_object_get_name` returning **empty** for every one of
our threads (object class `0x0b01…`, POSIX threads), which under §8.0's stated
reading — *"a nameless listing means the naming half failed"* — would have been
a failure. It is not. `pthread_getname_np` returns the names correctly on the
threads themselves (`kname=cbLow`, `kname=CAS-TCP`, …), and re-running the dump
through `_Thread_Get_name` — which is what `pthread_setname_np` actually writes
— shows every name. `rtems_object_get_name` reads the classic `Objects_Name`,
which a POSIX thread never has.

So: the names reach the kernel; a listing built on `rtems_object_get_name`
alone cannot see them. Truncation is as designed — `CAS-client-bloc`,
`CAS-event-block`, 15 bytes.

`0x0b01000a` at core 254 is the probe's own dump thread: it was spawned with
`thread::Builder` without `enter_ioc_thread`, so it inherits `POSIX_Init`'s
band and carries no kernel name. That is §5.9 Reason 3 reproduced by accident —
a thread that skips the prologue runs one level above idle — and it is
scaffolding, not production.

## Before / after

The pre-banding image `~/rtems-bringup/caioc.exe` **does still boot and serve**
(DHCP to 10.0.2.15, three records, `caget RTEMS:AO RTEMS:LO RTEMS:MSG`
returned `1.5` / `7` / `rtems-ca-ioc`). It yields **no priority numbers**: it
carries no readback path, and a prebuilt binary cannot be instrumented after
the fact. The before/after comparison is therefore boot-and-serve only — the
prior state of `CAS-TCP`/`CAS-UDP` was not measured and now cannot be without
rebuilding the pre-banding commit.

## Reproducing

```
ssh coding-agent@192.168.2.128
cd ~/epics-rs && git checkout -B measure-priority scope-b-priority
git apply <doc/rtems-priority-probe.patch>
~/rtems-bringup/prio/build-instr.sh      # PATH gets ~/rtems-bringup/tools/bin
~/rtems-bringup/prio/boot-instr.sh       # tcp 5064, udp 15076 -> guest 5064
EPICS_CA_ADDR_LIST=127.0.0.1:15076 EPICS_CA_AUTO_ADDR_LIST=NO camonitor RTEMS:AO &
EPICS_CA_ADDR_LIST=127.0.0.1:15076 EPICS_CA_AUTO_ADDR_LIST=NO camonitor RTEMS:LO &
grep 'PRIOPROBE\|TASKDUMP' ~/rtems-bringup/prio/after.log
```

Raw console log of the reading above: `~/rtems-bringup/prio/after.log` on the
box.
