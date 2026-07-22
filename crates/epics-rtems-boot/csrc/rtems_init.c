/*
 * POSIX_Init — the RTEMS entry task for an epics-rs IOC.
 *
 * Brings up the console, the clock, libbsd and DHCP, then calls the Rust
 * `main`. Derived from EPICS base's POSIX arm,
 * `modules/libcom/RTEMS/posix/rtems_init.c`; each step cites the base line it
 * comes from, and `doc/rtems-boot-shim-design.md` §1.1 records what base does
 * here that we deliberately drop (NFS/TFTP mounts, iocsh registration and the
 * startup script, telnetd, NTP, the NVRAM boot path, the i386 QEMU e1000 NVM
 * hack).
 *
 * The contract with Rust is one line: base's `main(argc, argv)` call at
 * :1183. rustc emits a C `main` that hands argc/argv to the Rust runtime, so
 * `std::env::args()` inside the IOC sees whatever is passed here.
 *
 * NOT COMPILED ON THIS MACHINE — see the note at the top of rtems_config.c.
 */

#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

#include <rtems.h>
#include <rtems/bsd/bsd.h>
#include <rtems/dhcpcd.h>
#include <rtems/printer.h>
#include <rtems/stackchk.h>

#include <machine/rtems-bsd-commands.h>

/* The Rust entry point (rustc emits this C symbol for `fn main`). */
extern int main(int argc, char **argv);

/*
 * How long POSIX_Init waits for a DHCP lease before giving up and booting
 * anyway. Base waits 600 s and then continues (:1066-1072); a long wait is the
 * wrong default here because rung 1 of the acceptance ladder — does the image
 * boot at all — does not need an address, and a board that hangs for ten
 * minutes on a dead network is a board that cannot be diagnosed.
 */
#ifndef EPICS_RTEMS_DHCP_TIMEOUT_SECONDS
#define EPICS_RTEMS_DHCP_TIMEOUT_SECONDS 30
#endif

/* Set to 0 for a quiet image once bring-up is over (base :1034). */
#ifndef EPICS_RTEMS_BSD_LOG_DEBUG
#define EPICS_RTEMS_BSD_LOG_DEBUG 1
#endif

/*
 * No RTC on this BSP: base guards CONFIGURE_APPLICATION_NEEDS_RTC_DRIVER out
 * for __arm__ and its comment says the RTC "seems to be missing with libbsd and
 * qemu" (base :960-964). So the clock starts from a compile-time constant and
 * is identical on every boot of every board. It is spelled out here, rather
 * than buried, because that determinism is a correctness hazard: nothing in the
 * IOC may derive an identity, a seed or a nonce from the wall clock on this
 * target.
 *
 * Base's value is 1397460606 (2014-04-14). Overridable so a run can be stamped.
 */
#ifndef EPICS_RTEMS_BOOT_EPOCH
#define EPICS_RTEMS_BOOT_EPOCH 1397460606
#endif

/*
 * Delay before dying, so the console actually flushes the message (base
 * :144-150). On a serial-scrape acceptance ladder a panic that loses its own
 * message is a wasted boot.
 */
static void delayedPanic(const char *msg)
{
    rtems_task_wake_after(rtems_clock_get_ticks_per_second());
    rtems_task_wake_after(rtems_clock_get_ticks_per_second());
    rtems_panic("%s", msg);
}

/*
 * Step 1 — console with no flow control (base :710-729). Base's four
 * diagnostic printfs at :715-718 are dropped; the flow-control clear is the
 * part that matters, because XON/XOFF on the console corrupts a serial scrape.
 */
static void initConsole(void)
{
    struct termios t;

    if (tcgetattr(fileno(stdin), &t) < 0) {
        printf("rtems-boot: tcgetattr failed: %s\n", strerror(errno));
        return;
    }
    t.c_iflag &= ~(IXOFF | IXON | IXANY);
    if (tcsetattr(fileno(stdin), TCSANOW, &t) < 0) {
        printf("rtems-boot: tcsetattr failed: %s\n", strerror(errno));
        return;
    }
}

/*
 * Step 5 — the payoff for CONFIGURE_STACK_CHECKER_ENABLED (base :818-827):
 * on exit, print per-task stack high-water marks. This is what turns the
 * acceptance ladder's resource rung into a measurement instead of a guess.
 */
static void report_stack_usage(int exit_code, void *arg)
{
    rtems_printer printer;

    (void)exit_code;
    (void)arg;

    rtems_print_printer_printf(&printer);
    rtems_stack_checker_report_usage_with_plugin(&printer);
}

/*
 * Step 10 — the DHCP hook.
 *
 * Base parses seven variables out of the dhcpcd environment (NTP servers, TFTP
 * server, bootfile, kernel command line) into globals (:742-811); every one of
 * those feeds a facility §1.1 drops. What remains is the two things we use: the
 * BOUND edge, and the hostname.
 */
static volatile int dhcp_bound = 0;

static void dhcpcd_hook_handler(rtems_dhcpcd_hook *hook, char *const *env)
{
    const char *reason = NULL;
    const char *host = NULL;

    (void)hook;

    for (; NULL != *env; ++env) {
        printf("rtems-boot: dhcpcd --> %s\n", *env);
        if (strncmp(*env, "reason=", 7) == 0) {
            reason = *env + 7;
        } else if (strncmp(*env, "new_host_name=", 14) == 0) {
            host = *env + 14;
        }
    }

    if (reason != NULL && strcmp(reason, "BOUND") == 0) {
        if (host != NULL && host[0] != '\0') {
            sethostname(host, strlen(host));
        }
        printf("rtems-boot: dhcp BOUND\n");
        dhcp_bound = 1;
    }
}

static rtems_dhcpcd_hook dhcpcd_hook = {.name = "epics-rs boot",
                                        .handler = dhcpcd_hook_handler};

/*
 * dhcpcd needs its config file to exist (base :838-875). Base's own guard is
 * `if (ENOENT == stat(...))`, which compares a status code against a return
 * value and is therefore never true; written correctly here. The file itself is
 * minimal because every option base requests (ntp-servers, tftp-server-name,
 * bootfile-name, rtems_cmdline) feeds a dropped facility.
 */
static void start_dhcpcd(void)
{
    static const char cfg[] = "nodhcp6\nipv4only\n";
    struct stat statbuf;
    rtems_status_code sc;

    if (stat("/etc/dhcpcd.conf", &statbuf) != 0) {
        int fd = open("/etc/dhcpcd.conf", O_CREAT | O_WRONLY, S_IRUSR | S_IWUSR);
        if (fd < 0) {
            delayedPanic("cannot create /etc/dhcpcd.conf");
        }
        if (write(fd, cfg, sizeof(cfg) - 1) != (ssize_t)(sizeof(cfg) - 1)) {
            delayedPanic("cannot write /etc/dhcpcd.conf");
        }
        if (close(fd) != 0) {
            delayedPanic("cannot close /etc/dhcpcd.conf");
        }
    }

    sc = rtems_dhcpcd_start(NULL);
    if (sc != RTEMS_SUCCESSFUL) {
        delayedPanic("rtems_dhcpcd_start failed");
    }
}

void *POSIX_Init(void *argument)
{
    struct timespec now;
    rtems_status_code sc;
    rtems_task_priority old_prio;
    char *argv[] = {"rtems-ioc", NULL};
    int result;
    int waited;

    (void)argument;

    /* 1. Console first, so every message below is actually readable. */
    initConsole();
    printf("\nrtems-boot: POSIX_Init entered (RTEMS %s)\n",
           rtems_get_version_string());

    /* 2. Fixed boot clock — see EPICS_RTEMS_BOOT_EPOCH above (base :965-972). */
    now.tv_sec = EPICS_RTEMS_BOOT_EPOCH;
    now.tv_nsec = 0;
    if (clock_settime(CLOCK_REALTIME, &now) < 0) {
        printf("rtems-boot: cannot set time: %s\n", strerror(errno));
    }

    /*
     * 3/6. Get off the init task's default priority (base :1000-1002, then
     * :829-836 called at :1038). Base does this twice — once to the iocsh
     * priority, then lower again before libbsd comes up. We have no
     * epicsThread priority mapping to reach for, and base's second value is
     * strictly the lower of the two, so the two steps collapse into one: run
     * the init task just above idle, and let libbsd's background work outrank
     * it.
     *
     * This value is INHERITED, and that is the whole reason it matters.
     * `_POSIX_Threads_Default_attributes` sets
     * `inheritsched = PTHREAD_INHERIT_SCHED`
     * (`cpukit/posix/src/pthreadattrdefault.c:49-58`), and Rust's `std` never
     * calls `pthread_attr_setinheritsched`, so a thread created from here gets
     * *this* priority rather than one of its own. Base does not have that
     * problem because `epicsThreadCreate` sets `PTHREAD_EXPLICIT_SCHED` on the
     * attribute set (`libcom/src/osi/os/posix/osdThread.c:158-166`); base's
     * init task stays at this same `RTEMS_MAXIMUM_PRIORITY - 1` for the rest
     * of its life (`libcom/RTEMS/posix/rtems_init.c:1038`, never raised again)
     * and calls `main()` from it, exactly as we do.
     *
     * So do NOT raise this value to rescue a thread that forgot its band:
     * lowering here is base parity and is what lets libbsd's background work
     * outrank the init task. The rule is the other half instead —
     * *every* IOC thread takes its own band on itself, as its first statement,
     * through `epics_base_rs::runtime::task::enter_ioc_thread`, which since
     * `52784cb4` really does call `pthread_setschedparam` on this target. A
     * thread that skips that prologue does not run "at the default"; it runs
     * here, one level above idle. Each server crate's `blocking.rs` carries
     * the source guard that keeps the set of such threads empty.
     *
     * The one thread that legitimately stays here is this one: after `main()`
     * has started the IOC it only waits, and C's init task — which goes on to
     * run iocsh — is at the same level for the same reason.
     */
    sc = rtems_task_set_priority(RTEMS_SELF, RTEMS_MAXIMUM_PRIORITY - 1U,
                                 &old_prio);
    if (sc != RTEMS_SUCCESSFUL) {
        delayedPanic("cannot lower the init task priority");
    }

    /* 4. Verbose stack logging during bring-up (base :1034). */
#if EPICS_RTEMS_BSD_LOG_DEBUG
    rtems_bsd_setlogpriority("debug");
#endif

    /* 5. Stack-usage report on exit (base :1035). */
    on_exit(report_stack_usage, NULL);

    /* 7. The network stack comes up here (base :1040-1041). */
    printf("rtems-boot: initializing libbsd\n");
    sc = rtems_bsd_initialize();
    if (sc != RTEMS_SUCCESSFUL) {
        delayedPanic("rtems_bsd_initialize failed");
    }

    /* 8. Let the callout timer allocate its resources (base :1043-1045). */
    sc = rtems_task_wake_after(2);
    if (sc != RTEMS_SUCCESSFUL) {
        delayedPanic("rtems_task_wake_after failed");
    }

    /*
     * 9. Loopback (base :1047-1048). This is what makes an in-guest
     * EPICS_CA_ADDR_LIST=127.0.0.1 meaningful, so the ladder can test the
     * protocol before it tests the network.
     */
    rtems_bsd_ifconfig_lo0();

    /* 10. DHCP, bounded — boot anyway on timeout, loudly (base :1050-1072). */
    rtems_dhcpcd_add_hook(&dhcpcd_hook);
    start_dhcpcd();
    for (waited = 0; !dhcp_bound && waited < EPICS_RTEMS_DHCP_TIMEOUT_SECONDS;
         ++waited) {
        rtems_task_wake_after(rtems_clock_get_ticks_per_second());
    }
    if (!dhcp_bound) {
        printf("rtems-boot: ***** DHCP did not bind in %d s; continuing with no "
               "address. Server ports will bind but nothing off-board can reach "
               "them. *****\n",
               EPICS_RTEMS_DHCP_TIMEOUT_SECONDS);
    }

    /*
     * Base keeps these two dumps (:1081-1084) and so do we: when the guest is
     * unreachable, knowing its address and routes is the whole diagnosis.
     */
    {
        char *ifconfig_argv[] = {"ifconfig", NULL};
        char *netstat_argv[] = {"netstat", "-rn", NULL};

        printf("rtems-boot: -------- ifconfig --------\n");
        rtems_bsd_command_ifconfig(1, ifconfig_argv);
        printf("rtems-boot: -------- netstat -rn --------\n");
        rtems_bsd_command_netstat(2, netstat_argv);
    }

    /* 11. The Rust IOC (base :1183-1191). */
    printf("rtems-boot: main() reached\n");
    result = main((int)(sizeof(argv) / sizeof(argv[0])) - 1, argv);
    printf("rtems-boot: IOC terminated with %d\n", result);

    exit(result);
    delayedPanic("returned from exit()");
    return NULL;
}
