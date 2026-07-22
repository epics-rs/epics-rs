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
