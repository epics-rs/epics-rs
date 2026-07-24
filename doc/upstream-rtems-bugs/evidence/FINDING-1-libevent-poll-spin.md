# libevent's poll backend busy-spins on RTEMS 6 / rtems-libbsd

Measured on: RTEMS 6.0.0 (`2faafecb7f9df8400fd78a1e6d9b3cf3df0eeccc`), BSP
`xilinx_zynq_a9_qemu`, arm-rtems6-gcc 13.3.0, qemu-system-arm, 256 MB guest,
EPICS base 7.0.10, pvxs `cc7bc72` with bundled libevent `1fe626c4`.

## Symptom as originally seen

A pvxs-based IOC never reaches `iocInit`.  It wedges inside
`pvxsBaseRegistrar()` at 100% guest CPU.  pvxs's own `test/testev.cpp`
`test_call()` hangs identically.

## What it actually is

`event_base_loop()` never blocks when libevent selects its **poll** backend.
It returns from `poll()` immediately and re-enters, at ~27 us per iteration.

Measured with a `-Wl,--wrap=poll` interposer (probe binary only; libevent and
pvxs sources unmodified) around a 4.000 s `event_base_loop`, base armed with a
single 1000 s timer plus a 4 s `event_base_loopexit`:

| backend        | poll() calls in 4 s | timeouts requested | guest IDLE |
|----------------|--------------------:|-------------------:|-----------:|
| raw `poll()`   |                   1 | 4000 ms            |     97.9 % |
| libevent kqueue|                   0 | -                  |     97.7 % |
| libevent poll  |         **148 081** | min 1 ms, max 4000 |     33.6 % |

IDLE is from `rtems_cpu_usage_report()`.  In the poll case the (now-exited)
loop thread accounts for the missing 3.95 s of a 4.00 s window: it burned the
core for the entire duration of what the caller sees as a correct 4 s block.

## Root cause

libevent's internal notification channel is created by
`evutil_make_internal_pipe_()`, which prefers `pipe()`/`pipe2()` over
`socketpair()`.  On this target `pipe()` returns an RTEMS IMFS FIFO, not a
libbsd socket.  rtems-libbsd's `poll()` cannot wait on such a descriptor: it
returns immediately with `POLLERR`.

Measured directly, no libevent involved:

```
fd 55: getsockopt(SO_TYPE)=0  type=2   mode=0140666   [udp socket]
    poll(fd=55, POLLIN, 1000 ms) rv=0 revents=0x0  elapsed=1.0106 s  BLOCKS

pipe() -> read fd 56, write fd 57
fd 56: getsockopt(SO_TYPE)=-1 errno=9  mode=010600    [IMFS FIFO]
    poll(fd=56, POLLIN, 1000 ms) rv=1 revents=0x8  elapsed=0.0002 s  IMMEDIATE
                                        ^^^^^^^^^^ POLLERR

socketpair(AF_UNIX) -> 58, 59
fd 58: getsockopt(SO_TYPE)=0  type=1   mode=0140666   [unix socket]
    poll(fd=58, POLLIN, 1000 ms) rv=0 revents=0x0  elapsed=1.0058 s  BLOCKS
```

and inside the spinning loop the interposer records exactly this descriptor:

```
poll() calls=69076  last: fd=62 msec=1 rv=1 revents=0x8 errno=11
fd 62: getsockopt(SO_TYPE)=-1 errno=9  fstat=-1        [libevent notify fd]
```

`revents=0x8` is `POLLERR`.  libevent treats the notify fd as always ready,
so `poll()` is asked for >=1 ms and returns in ~27 us, forever.

## Why it presents as "a secondary thread hangs"

The spin is present on any thread; it is only *observable* when some other
thread needs the core.  RTEMS POSIX threads on this BSP default to
`SCHED_FIFO` priority 1, and `pthread_create` inherits the creator's priority,
so the loop thread and the thread that created it are equal-priority on a
single core.  SCHED_FIFO does not time-slice between equals, so the spinner
never yields:

| case                                   | 20 x nanosleep(100 ms) on the other thread |
|----------------------------------------|-------------------------------------------:|
| worker `nanosleep(4 s)`                 | 2.200 s  normal |
| worker `poll(NULL,0,4000)`              | 2.192 s  normal |
| worker `poll(1 udp fd,4000)`            | 2.194 s  normal |
| worker `select(1 udp fd,4000)`          | 2.195 s  normal |
| worker libevent **kqueue** loop, 4 s    | 2.187 s  normal |
| worker libevent **poll** loop, 4 s      | **6.105 s  starved for the full 4 s** |
| ... + a real fd registered              | 6.089 s  starved |
| ... + `evthread_make_base_notifiable`   | 6.091 s  starved |

**Priority hypothesis: tested and rejected as the cause.**  Raising the other
thread above the loop thread (main `SCHED_FIFO` 3, worker explicitly 1) makes
the starvation disappear -- main returns to 2.198 s -- but guest IDLE in that
same window falls to **0.000074 s of 4.017 s**.  The core is still 100 %
consumed by the spin.  Priority separation hides the symptom; it does not fix
the defect.  This is a broken primitive, not priority-inversion starvation.

## Who owns it

**rtems-libbsd owns the defect.**  `poll()` on a valid, open, non-socket
descriptor must report the descriptor's real readiness, not `POLLERR`.
RTEMS routes `poll()` for IMFS objects through libio; a descriptor that
libbsd's `poll()` cannot map to a socket is being flagged as an error rather
than delegated.  A conforming `poll()` here would fix libevent, pvxs and any
other portable reactor at once.

**libevent is worth a defensive change.**  `evutil_make_internal_pipe_()`
could prefer `socketpair()` on RTEMS; `socketpair(AF_UNIX)` is measured above
to poll correctly on this exact target.  This would make the poll backend
usable even on an unfixed libbsd.

**pvxs is only the victim, but it holds the one-line fix.**
`src/evhelper.cpp:183` unconditionally does

```c
#ifdef __rtems__
    /* with libbsd circa RTEMS 5.1
     * TCP peer close/reset notifications appear to be lost.
     * Maybe due to absence of NOTE_EOF?
     * poll() seems to work though.  */
    event_config_avoid_method(conf.get(), "kqueue");
#endif
```

That RTEMS-5.1-era workaround is what steers pvxs into the broken backend on
RTEMS 6.

## Consequence: pvAccess does work on RTEMS 6

With that single line commented out and nothing else changed, the same pvxs
IOC boots to completion on the same guest:

```
iocRun: All initialization complete
```

and answers from the host over `EPICS_PVA_NAME_SERVERS=127.0.0.1:5175`:

* `pvxinfo PIOC:AI1` -> full `epics:nt/NTScalar:1.0` introspection
* `pvxget  PIOC:AI1` -> `value double = 1.25`
* `pvxput  PIOC:AO 7.5` then `pvxget PIOC:AO` -> `value double = 7.5`
* `pvxmonitor PIOC:HB100` -> streaming updates from a 0.1 s scan record

The reason the workaround was added was also re-tested: killing that monitor
client with `SIGKILL` (no FIN) is detected by the kqueue backend on RTEMS 6:

```
DEBUG pvxs.tcp.io connection to Client 10.0.2.2:34704 closed by peer
DEBUG pvxs.tcp.setup Client 10.0.2.2:34704 Cleanup TCP Connection
```

So the RTEMS 5.1 reason for avoiding kqueue does not reproduce on RTEMS 6,
and removing the avoid-kqueue call turns "no working pvAccess server on
RTEMS 6" into a working one.

## Reproduction

~90 lines of C, no pvxs: create an `event_base` with
`event_config_avoid_method(conf,"kqueue")`, add one `EV_PERSIST` 1000 s timer,
`event_base_loopexit(+4 s)`, run `event_base_loop(base,0)` on one thread while
another does `20 x nanosleep(100 ms)`.  The second thread takes ~6 s instead of
~2 s.  Sources: `~/rtems-cside/probe/probeApp/src/probeMain.c`.
