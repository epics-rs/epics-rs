/*
 * RTEMS application configuration for an epics-rs IOC.
 *
 * Derived from EPICS base's POSIX arm,
 * `modules/libcom/RTEMS/posix/rtems_config.c` (read in full); every directive
 * below cites the base line it comes from. What base configures and this file
 * deliberately does not: NFS, TFTP, telnetd, ftpd, libblock/BDBUF, the RTC
 * driver, the ~25-command shell.
 *
 * This is a *configuration* translation unit: `<rtems/confdefs.h>` at the
 * bottom turns the `CONFIGURE_*` macros above it into the actual RTEMS object
 * tables, so nothing may follow it and the order of the includes matters. The
 * ordering here mirrors base's, which is known to build.
 *
 * NOT COMPILED ON THIS MACHINE: no arm-rtems6 toolchain and no RTEMS headers
 * exist on the development host, so this file is type-checked by nobody until
 * the bring-up box builds it. The host-side tests in `src/contract.rs` guard
 * its *structure* (entry point, confdefs last, the user-extension count, the
 * absence of the dropped facilities) — not its compilability.
 */

#include <rtems.h>

/*
 * A. The entry contract (base :22-26).
 *
 * `POSIX_Init` is defined in rtems_init.c. RTEMS >= 5 uses the POSIX API arm
 * (base `configure/toolchain.c:31-35`), which is also what Rust needs: its
 * `std::thread` is pthreads.
 */
extern void *POSIX_Init(void *argument);

#define CONFIGURE_POSIX_INIT_THREAD_TABLE
#define CONFIGURE_POSIX_INIT_THREAD_ENTRY_POINT POSIX_Init
#define CONFIGURE_POSIX_INIT_THREAD_STACK_SIZE (64 * 1024)

/*
 * D. Tick period (base :34-36), kept at base's value and left overridable from
 * the build so a timing experiment needs no source edit. This 10 ms quantum is
 * what `std::thread::sleep`, the delayed-timer band and every latency number
 * the acceptance ladder can produce are rounded to.
 *
 * This define is the SINGLE SOURCE OF TRUTH for the tick rate, including on the
 * Rust side: `epics_base_rs::runtime::time::thread_sleep_quantum` reads it back
 * through `sysconf(_SC_CLK_TCK)` -> `rtems_clock_get_ticks_per_second()` ->
 * `_Watchdog_Ticks_per_second` = `1000000 / CONFIGURE_MICROSECONDS_PER_TICK`,
 * so overriding it here moves record DLY quantization with it. Do not restate
 * this number as a constant anywhere in Rust — a guard test in that module
 * fails if anyone does.
 */
#ifndef CONFIGURE_MICROSECONDS_PER_TICK
#define CONFIGURE_MICROSECONDS_PER_TICK 10000
#endif

/*
 * E. Task stack pool (base :39). Every Rust thread's stack comes out of this,
 * and this port is thread-per-connection: CA runs one thread per client, PVA
 * three (the 3N+2 budget in `server_native/blocking.rs`). Base's generous value
 * is kept; shrinking it is an optimisation to make after the acceptance
 * ladder's `stackuse` rung measures real usage, not before.
 */
#define CONFIGURE_EXTRA_TASK_STACKS (4000 * RTEMS_MINIMUM_STACK_SIZE)

/*
 * K. Filesystems (base :42, :46, :49). A writable root is required because
 * dhcpcd writes /etc/dhcpcd.conf; DEVFS is what provides /dev/console — and is
 * where /dev/urandom would appear if this BSP has one, which is the open
 * question behind the server-GUID entropy arm.
 */
#define CONFIGURE_FILESYSTEM_DEVFS
#define CONFIGURE_FILESYSTEM_IMFS
#define CONFIGURE_USE_IMFS_AS_BASE_FILESYSTEM

/*
 * J. libbsd (base :57-59). This is what puts the network stack in the image;
 * without it Rust's `std::net` links against nothing. Must precede confdefs.
 */
#define RTEMS_BSD_CONFIG_BSP_CONFIG
#define RTEMS_BSD_CONFIG_INIT
#include <machine/rtems-bsd-config.h>

/*
 * B, C. Drivers (base :65-66). No clock driver means no ticks, so
 * `rtems_task_wake_after`, every `std::thread::sleep` and the delayed-timer
 * band never fire. The console is the acceptance instrument: every rung of the
 * ladder scrapes stdout off the serial line.
 */
#define CONFIGURE_APPLICATION_NEEDS_CLOCK_DRIVER
#define CONFIGURE_APPLICATION_NEEDS_CONSOLE_DRIVER

/*
 * Block-device buffering. MEASURED: dropping this does not drop libblock from
 * the image, it only drops libblock's *configuration*, and the link then dies
 * with "undefined reference to `rtems_bdbuf_configuration'" from
 * librtemscpu.a(bdbuf.c.70.o). RTEMS_BSD_CONFIG_BSP_CONFIG above pulls in this
 * BSP's nexus devices, which include the two Arasan SDHCI controllers, and the
 * SD/MMC stack references bdbuf unconditionally. confdefs/bdbuf.h:54,133 (rtems_6) only
 * defines rtems_bdbuf_configuration under this macro, so on a BSP whose nexus
 * device set contains a block device the directive is not optional.
 */
#define CONFIGURE_APPLICATION_NEEDS_LIBBLOCK

/*
 * F. File-descriptor ceiling (base :83, with base's own caveat at :70-81).
 *
 * Base caps this at 64 solely to stay below newlib's FD_SETSIZE, because
 * `select()` on a descriptor at or above FD_SETSIZE faults — and base's comment
 * says outright that "IOC core components (libca and RSRV) do not make
 * select() calls". Neither do we: an `rg 'libc::select|libc::poll|FD_SET'`
 * across epics-base-rs, epics-ca-rs and epics-pva-rs returns zero hits, because
 * this port is blocking thread-per-connection with no reactor anywhere. Base's
 * own score arm sets 150 (`score/rtems_config.c:36`), so that is the value
 * taken here — 150 is base's own number, not one invented for this shim.
 *
 * But base does not compile both arms. configure/toolchain.c:32-35 selects
 * OS_API = posix when __RTEMS_MAJOR__ >= 5, and RTEMS/Makefile:15 (`SRC_DIRS +=
 * ../$(OS_API)`) with :27 pulls rtems_config.c out of the arm that selected.
 * So an RTEMS 6 build of base compiles the POSIX arm, and the ceiling base
 * ACTUALLY RUNS WITH on this target is 64. The deviation is which arm's number
 * we run, not the number: we run the score arm's 150 where base runs the POSIX
 * arm's 64.
 *
 * This is the binding constraint on concurrent clients: one connection is one
 * descriptor however many threads serve it.
 *
 * VERIFIED on the bring-up box: CONFIGURE_MAXIMUM_FILE_DESCRIPTORS is the
 * correct RTEMS 6 spelling. confdefs/libio.h:89 (rtems_6) is what reads it, and
 * confdefs/obsolete.h:109-111 (rtems_6) turns the older CONFIGURE_LIBIO_MAXIMUM_FILE_-
 * DESCRIPTORS into a #warning that it "has been renamed to
 * CONFIGURE_MAXIMUM_FILE_DESCRIPTORS since RTEMS 5.1".
 *
 * The FD_SETSIZE worry is settled too, and does not bind: newlib's
 * sys/select.h:33-34 takes the __rtems__ arm and defines FD_SETSIZE 256
 * (confirmed by preprocessing with the real BSP include path), so 150 is under
 * the ceiling even for a library that does call select(). That measurement is
 * the load-bearing half — it holds whatever our own code does, and only a cap
 * above 256 would re-open the question. Do not re-litigate it.
 *
 * THE VALUE BELOW IS NOT HARD-CODED. The #ifndef wrapper is deliberate: any
 * -D CONFIGURE_MAXIMUM_FILE_DESCRIPTORS=N reaching this file's compile line
 * wins, so the box can bisect the ceiling without a source edit. The fd=400
 * image whose memory-wall number appears below is an image with a different
 * cap; this wrapper is the route by which one exists.
 *
 * ---------------------------------------------------------------------------
 * THIS IS A DEVIATION: we run base's score-arm 150 on a target where base
 * itself compiles the POSIX arm and runs 64 (see the arm selection above).
 * Every measurement behind it is below.
 * ---------------------------------------------------------------------------
 *
 * MEASURED on the bring-up box, identical driver (raw CA TCP, version
 * handshake, one CA_PROTO_CREATE_CHAN, "served" only on reply 18):
 *
 *   stock EPICS base, cap 64        -> 53 served, #54 refused
 *   base rebuilt at our cap 150     -> 139 served, #140 refused
 *   epics-rs, cap 150               -> 142 served, #143 refused
 *
 * Same console line and same errno on both stacks — "[zone: socket]
 * kern.ipc.maxsockets limit reached" / "CAS: Client accept ERROR: Too many
 * open files in system" (ENFILE). C is 3 lower at the same cap only because it
 * holds 3 more descriptors itself at idle, not because it serves worse.
 *
 * TWO WALLS, and 142 is not the memory one:
 *
 *   fd wall     = MAXIMUM_FILE_DESCRIPTORS - 8 = 142   (unchanged by more RAM)
 *   memory wall = free heap / 1,589,000 B     = 151   (roughly doubles w/ RAM)
 *
 * The 8 is what the IOC itself holds at idle; the status PVs confirm it
 * (FD_CNT + FD_FREE = FD_MAX = 150 on every row, FD_FREE = 142 at zero
 * connections), and so does the errno being ENFILE rather than ENOMEM. The
 * effective ceiling is the LOWER of the two, so raising either one alone buys
 * almost nothing.
 *
 * DO NOT RE-RUN THE 300-CONNECTION RAMP TO FIND THIS OUT: it was already run on
 * an image with this cap at 400, where the fd wall no longer binds. Memory then
 * binds at 151 served, with EAGAIN (thread creation) refusals instead of
 * ENFILE, by two independent derivations (300 attempted / 149 refused, and
 * (259,803,736 - 19,880,696)/1,589,000 = 150.99). So raising this cap buys
 * NINE connections, 142 -> 151, and then the 256 MB guest is out of heap.
 */
#ifndef CONFIGURE_MAXIMUM_FILE_DESCRIPTORS
#define CONFIGURE_MAXIMUM_FILE_DESCRIPTORS 150
#endif

/*
 * User extensions (base :87 sets 5).
 *
 * MEASURED: a shim without this directive does not boot. libbsd fails during
 * early init with `emerg: rtems_bsd_threads_init_early: cannot create
 * extension`, because CONFIGURE_UNLIMITED_OBJECTS does not cover user
 * extensions. libbsd's own testsuite default-init.h reserves 1, which is the
 * value proven to boot on this BSP.
 *
 * MEASURED again with CONFIGURE_STACK_CHECKER_ENABLED on: 1 is still enough.
 * The image boots and the exit-time stack-usage report prints, because the
 * stack checker is installed as an *initial* extension out of the statically
 * generated table rather than through rtems_extension_create(), and this
 * directive sizes only the runtime-created pool that libbsd draws its one
 * extension from. If a future image does fail to create an extension, raise
 * this first; base reserves 5.
 */
#ifndef CONFIGURE_MAXIMUM_USER_EXTENSIONS
#define CONFIGURE_MAXIMUM_USER_EXTENSIONS 1
#endif

/*
 * H, I. Object tables and heap (base :89-91). Unlimited objects is what makes
 * thread-per-connection viable at all — a fixed task table would cap clients at
 * a compile-time constant. Unified work areas keeps the RTEMS object heap and
 * the Rust allocator from being two pools we would both have to size right.
 */
#define CONFIGURE_UNLIMITED_ALLOCATION_SIZE 32
#define CONFIGURE_UNLIMITED_OBJECTS
#define CONFIGURE_UNIFIED_WORK_AREAS

/*
 * G. Stack checking (base :93). The single most valuable directive for a Rust
 * port: Rust's own stack-guard machinery is thin on a tier-3 target, so without
 * this a blown thread stack is silent memory corruption rather than a report.
 * POSIX_Init installs the exit hook that prints the usage report (base
 * :818-827), which is what makes the ladder's stack rung a measurement.
 */
#define CONFIGURE_STACK_CHECKER_ENABLED

/*
 * L. A reduced shell (base :112-158 configures ~25 commands; these are the four
 * the acceptance ladder actually uses). netstat is the "is the server
 * listening" rung, ifconfig is the diagnosis path when the guest is
 * unreachable, stackuse and malloc_info are the resource rung.
 */
#define CONFIGURE_SHELL_COMMANDS_INIT

#include <rtems/netcmds-config.h>

#define CONFIGURE_SHELL_USER_COMMANDS                                          \
    &rtems_shell_NETSTAT_Command, &rtems_shell_IFCONFIG_Command

#define CONFIGURE_SHELL_COMMAND_STACKUSE
#define CONFIGURE_SHELL_COMMAND_MALLOC_INFO

#include <rtems/shellconfig.h>

/*
 * M. Driver table (base :173). libbsd, the console and the shell each register
 * several, and the slots are cheap.
 */
#define CONFIGURE_MAXIMUM_DRIVERS 40

/*
 * N. confdefs generates the tables from everything above, so these two lines
 * are last (base :192-194).
 *
 * Note what is absent: CONFIGURE_APPLICATION_NEEDS_RTC_DRIVER. Base guards it
 * out for __arm__ (:180-184) and its own comment says the RTC "seems to be
 * missing with libbsd and qemu" (rtems_init.c:960). That is the mechanism
 * behind the fixed boot clock in rtems_init.c, and therefore behind the rule
 * that nothing may derive an identity from the wall clock on this target.
 */
#define CONFIGURE_INIT

#include <rtems/confdefs.h>
