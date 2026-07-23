# rtems-cside: deviations from stock upstream source

apt packages installed by this panel: NONE.  No sudo was used at any point.
Everything below lives under ~/rtems-cside/.  ~/rtems-bringup/, ~/epics-rs,
~/pvaprobe were read only (the RTEMS toolchain was COPIED to
~/rtems-cside/tools, verified byte-identical for the BSP tree with diff -rq).

## EPICS base R7.0.10 (bf11a0c)  --  ZERO source patches

Site configuration only (not source):
1. configure/CONFIG_SITE.local (new file)
   CROSS_COMPILER_TARGET_ARCHS = RTEMS-xilinx_zynq_a9_qemu
2. configure/os/CONFIG_SITE.Common.RTEMS (appended)
   RTEMS_VERSION = 6
   RTEMS_BASE = /home/coding-agent/rtems-cside/tools
   RTEMS_BSD_NETWORKING = yes
These are the settings the file exists to hold.  They make the tree report
"R7.0.10-dirty"; no .c/.cpp/.h was touched.

## C IOC application (~/rtems-cside/cioc)  --  app code, not base

3. ciocApp/src/ciocFsHook.c overrides base's WEAK hook
   epicsRtemsMountLocalFilesystem() to load the compiled-in memory filesystem
   and point argv[1] at /st.cmd.  base declares this hook for exactly this.
4. ciocApp/src/ciocRtemsConfig.c is a COPY of
   base/modules/libcom/RTEMS/posix/rtems_config.c with ONE line changed:
       CONFIGURE_MAXIMUM_FILE_DESCRIPTORS  64 -> 150
   linked into the app so it overrides the librtemsCom.a member.  This exists
   only for the fd=150 run that matches the epics-rs guest.  The fd=64 run
   (cioc-fd64.exe) is stock base configuration.
   *** THIS IS A DELIBERATE DEVIATION.  Stock base ships 64. ***

## pvxs (cc7bc72) + bundled libevent (1fe626c4)  --  ONE build-system patch

5. bundle/cmake/Platform/RTEMS.cmake line 28: removed "-specs bsp_specs".
   Original saved at ~/rtems-cside/patches/RTEMS.cmake.orig.
   Reason: RTEMS 6 no longer installs bsp_specs; -qrtems alone is correct.
   Without this, CMake cannot compile a single file:
     arm-rtems6-gcc: fatal error: cannot read spec file 'bsp_specs'
   No pvxs C++ source was patched.
6. configure/RELEASE.local, configure/CONFIG_SITE.local: paths + target arch
   + CROSS_COMPILER_RUNTEST_ARCHS (to get the RTEMS test executables built).

---

## Session 2 additions

### pvxs source patch #2 (measurement-relevant, MUST be declared)

`pvxs/src/evhelper.cpp:183` -- the RTEMS `event_config_avoid_method(conf, "kqueue")`
call is commented out.  Original preserved at `patches/evhelper.cpp.orig`.

This is NOT a build fix: it changes which libevent backend pvxs uses on RTEMS,
and it is the difference between "PVA IOC wedges before iocInit" and "PVA IOC
boots and serves".  Any pvxs measurement taken after 2026-07-22 was taken with
this line disabled.  See FINDING-1-libevent-poll-spin.md.

### Application-level additions to my C IOC (not patches to stock base)

* `cioc/ciocApp/src/ciocNetStat.c` + `.dbd` -- iocsh `bsdmbuf` (`netstat -m`)
  and `bsdzone` (`vmstat -z`), calling rtems-libbsd's own commands.
* `cioc/ciocApp/src/ciocSizes.c` + `.dbd` -- iocsh `casizes`, printing
  `sizeof(struct client)`, `MAX_TCP`, `sizeof(channel_in_use)`,
  `sizeof(event_ext)` from rsrv's private `server.h` via `-I` only.
* `fsbug/` -- 12-line application reproducing the base boot crash on
  *unpatched* base.  Contains no base modification.
* `probe/probeApp/src/Makefile` -- `USR_LDFLAGS += -Wl,--wrap=poll`, applied to
  the probe binary only, to count libevent's poll() calls.  No library or
  upstream source is affected.

### Base: still zero source patches.
### Package installs: still none; `sudo` has not been used this session.

---

## Session 3 additions (2026-07-22, thread-priority measurement)

### Base: still zero source patches.  pvxs: not built, not touched this session.
### Package installs: still none; `sudo` has still not been used.

The measurement was taken on **`cioc-fd64.exe`**, i.e. the image that uses base's
own `modules/libcom/RTEMS/posix/rtems_config.c` (64 descriptors).  The fd=150
deviation declared in item 2 above was deliberately NOT in play for these
numbers.  Deviations still present in that image are items 1 (site config),
3 (`ciocFsHook.c`, base's own WEAK hook) and the app sources of session 2 --
none of which is on any code path that sets or reads a thread priority.

No new deviation was applied.  Everything read was read with commands upstream
base and RTEMS already ship:

* `epicsThreadShowAll 1`   -- base, `libComRegister.c:523`; on RTEMS-posix it
  prints OSIPRI and a LIVE `pthread_getschedparam` readback (OSSPRI), plus the
  measured OSD priority range.
* `rt stackuse` / `rt top`  -- base's `rt` bridge to the RTEMS shell
  (`rtems_init.c:500`), running `rtems_shell_STACKUSE_Command` and
  `rtems_shell_TOP_Command`.  `top` is interactive; ENTER was fed to it through
  the same console fifo to make it exit.

### Host-side driver scripts added under ~/rtems-cside (not target source)

* `hold.py`, `hold2.py`, `hold3.py`, `hold4.py`, `hold5.py`, `hold6.py` --
  raw-CA-socket load drivers, same connect method as `ceiling.py`.  They open
  connections, subscribe, read, and drive iocsh through the `ciocin` fifo.
  Nothing is compiled into, or linked against, the target image.

Ports: 5164 only, as this panel owns.  The other panel's qemu (5064/15076,
pid 2062470) was neither touched nor read.  The qemu this session started was
`cioc-fd64.exe` as pid 2061756; no process this panel did not start was signalled.

---

## Session 4 additions (2026-07-23, epicsEvent heap-leak measurement)

### Base: still zero source patches.  pvxs: not built, not touched this session.
### Package installs: still none; `sudo` has still not been used.

The IOC connect/disconnect-cycle measurement was taken on **`cioc-fd64.exe`**,
byte-identical to the session-3 image
(`sha256 10a4db99c63159423a4d7bda2d6db5f1d57dcf73f6a7dc59d5aabc8f19e3efa1`),
with **no addition whatsoever**.  The heap instrument used is stock:

* `rt malloc` -- base's `rt` bridge to the RTEMS shell (`rtems_init.c:500`)
  running RTEMS 6's own `malloc [walk]` command, which prints the full heap
  statistics block (free/used block counts, total bytes free/used, lifetime
  allocation and free counters).  Found via `rt help mem`.  Nothing was added
  to the target image to read the heap.
* `epicsThreadShowAll 1` -- base, `libComRegister.c:523`, for the thread census.

### New image: `cioc-evloop-fd64.exe`
### sha256 `964bcbf64d59ba522d689a2fd7725cd9608dbc171650bcfe2092c01301f24bc0`

*** THIS IMAGE IS NOT STOCK.  It carries one added application source file. ***
It exists only to isolate the size of the leaked block directly; it does NOT
carry any of the connect/disconnect-cycle numbers, which come from
`cioc-fd64.exe`.

* `cioc/ciocApp/src/ciocEvLoop.c` + `.dbd` -- APP code, same shape as the
  session-2 `ciocSizes.c`.  Registers two iocsh commands:
    `evloop N` -- N x (`epicsEventCreate` + `epicsEventDestroy`), nothing else,
                  using only base's public `epicsEvent.h` API.
    `evsize`   -- prints `sizeof(rtems_binary_semaphore)` from `<rtems/thread.h>`.
  No EPICS base or RTEMS source is patched, copied-and-edited, or overridden by
  this file.
* `cioc/ciocApp/src/Makefile` -- two lines added (`cioc_DBD += ciocEvLoop.dbd`,
  `cioc_SRCS += ciocEvLoop.c`).
* `cioc/ciocApp/src/ciocRtemsConfig.c` -- `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS`
  set back to base's stock **64** for this build, so the evloop image differs
  from `cioc-fd64.exe` by the added command and nothing else.  (The tree had
  been left at the 150 of deviation item 2 by session 2.)

### Host-side driver added under ~/rtems-cside (not target source)

* `evleak.py` -- sequential CA connect/disconnect driver, concurrency exactly 1,
  60 warm-up cycles then baseline, then batches of 250 and 600, taking a
  `rt malloc` + `epicsThreadShowAll 1` reading through the `ciocin` fifo at each
  boundary.  Connect method is `hold6.py`'s, unchanged.  Nothing in it is
  compiled into or linked against the target image.

### Raw console logs kept
* `log/cioc-evleak-2026-07-23.log`  (25844 B) -- the `cioc-fd64.exe` run
* `log/cioc-evloop-2026-07-23.log`  (12190 B) -- the `cioc-evloop-fd64.exe` run

Ports: 5164 only, as this panel owns.  The other panel's guests (pids 2938228 /
2938252, ~/rtems-bringup/topoB, ports 4441/8011) were neither touched nor
signalled; no blanket `pkill` was ever issued.  The qemu processes this session
started were pid 2939153 (`cioc-fd64.exe`) and pid 2947898
(`cioc-evloop-fd64.exe`), both started and stopped only through
`boot-cioc.sh`'s own `qemu.pid` handling.

### Session 4, second boot (same day): repeat + monitor control

* `evmon.py` -- host-side driver, same discipline as `evleak.py`, but each cycle
  also creates a channel on `CIOC:HB100` and subscribes a monitor, closing with
  the subscription still live.  Control for the conditional "+1 block per
  cancelled monitor" path (`dbEvent.c:632`).  Not compiled into the image.
* `log/cioc-evleak-repeat-evmon-2026-07-23.log` (32415 B) -- one boot of the
  same stock `cioc-fd64.exe` carrying BOTH the repeat of the `evleak.py` run and
  the `evmon.py` control.  qemu pid 2952568.

NOTE on a stale label: `evleak.py` prints its first reading as
`T0-initial-after-20-smoke-cycles`.  That label is accurate for the FIRST boot
only (a 20-cycle smoke test had preceded it).  In this second boot no smoke test
was run, so that reading is a genuine pre-first-connection reading despite the
label.  The label was left unedited so the script in the repo is byte-identical
to the one that produced both logs.
