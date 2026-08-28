/*
 * POSIX_Init — the RTEMS entry task for an epics-rs IOC.
 *
 * Brings up the console, the clock, libbsd and the network — DHCP, or a
 * compile-time static address — then calls the Rust `main`. Derived from
 * EPICS base's POSIX arm,
 * `modules/libcom/RTEMS/posix/rtems_init.c`; each step cites the base line it
 * comes from, and `doc/rtems-boot-shim-design.md` §1.1 records what base does
 * here that we deliberately drop (NFS/TFTP mounts, iocsh registration and the
 * startup script, telnetd, NTP, the NVRAM boot path, the i386 QEMU e1000 NVM
 * hack).
 *
 * The contract with Rust is one line: base's `main(argc, argv)` call at
 * :1184. rustc emits a C `main` that hands argc/argv to the Rust runtime, so
 * `std::env::args()` inside the IOC sees whatever is passed here. That is the
 * whole configuration surface of a target image, so what this file puts in
 * argv is what a site can set: see `EPICS_RTEMS_CMDLINE` below, and
 * `src/boot_args.rs` for the meaning the IOC gives the tokens.
 *
 * NOT BUILT FOR THE TARGET ON THIS MACHINE — see the note at the top of
 * rtems_config.c. It is compiled FOR THE HOST on every push, against the
 * RTEMS declarations recorded in tests/rtems-api/, so a name, an arity or a
 * format string that is wrong here fails CI rather than the board's console;
 * that record's README.md says what the host compile does and does not prove.
 */

#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <net/if.h>
#include <net/if_dl.h>
#include <net/route.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sysexits.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

#include <rtems.h>
#include <rtems/bsd/bsd.h>
#include <rtems/dhcpcd.h>
#include <rtems/printer.h>
#include <rtems/stackchk.h>

#include <machine/rtems-bsd-commands.h>

#include "boot_args.h"

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
 * The boot command line — this image's `st.cmd`.
 *
 * Base names a startup script here (`bootp_cmdline_init`, base :103) and runs
 * it through iocsh; §1.1 drops both the script and the filesystems that carry
 * it, so the line itself has to carry the configuration. Its tokens are split
 * on whitespace into `argv[1..]` and given their meaning by the Rust side
 * (`src/boot_args.rs`): `NAME=VALUE` is `epicsEnvSet`, anything else is a file
 * to load. Without this the image had no configuration surface at all — the
 * compiled-in defaults were the only values it could ever have, and a site's
 * EPICS_CA_ADDR_LIST was ignored with no error.
 *
 * Compile-time default, overridden at run time by the DHCP option
 * `rtems_cmdline` exactly as base overrides its own (base :762). Build with
 *
 *     -DEPICS_RTEMS_CMDLINE="EPICS_CA_ADDR_LIST=10.0.2.2 /db/site.db"
 *
 * The buffer is larger than base's 128: base holds one pathname, this holds a
 * site's whole environment. It is never truncated — a half-copied
 * EPICS_CA_ADDR_LIST is a wrong address list, which is worse than a refused
 * one — so an oversized value is refused with a message, as base refuses its
 * own (base :789-791).
 */
#ifndef EPICS_RTEMS_CMDLINE
#define EPICS_RTEMS_CMDLINE ""
#endif

static char boot_cmdline[1024] = EPICS_RTEMS_CMDLINE;

static char *boot_argv[EPICS_RTEMS_MAX_BOOT_ARGS + 2];

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
 * server, bootfile, kernel command line) into globals (:742-811); five of those
 * feed a facility §1.1 drops. What remains is three things: the BOUND edge, the
 * hostname, and `rtems_cmdline` — which base takes as `new_rtems_cmdline`
 * (:762) into the same buffer its compile-time default lives in, and which is
 * how a site configures a board it cannot rebuild.
 *
 * Base requires the keys to arrive in order and ignores everything before it
 * has seen `reason=` (:786-788); this loop instead notes the values and commits
 * them once, after the loop, when the reason really was BOUND. Same outcome,
 * no dependence on dhcpcd's emission order.
 */
static volatile int dhcp_bound = 0;

static void dhcpcd_hook_handler(rtems_dhcpcd_hook *hook, char *const *env)
{
    const char *reason = NULL;
    const char *host = NULL;
    const char *cmdline = NULL;

    (void)hook;

    for (; NULL != *env; ++env) {
        printf("rtems-boot: dhcpcd --> %s\n", *env);
        if (strncmp(*env, "reason=", 7) == 0) {
            reason = *env + 7;
        } else if (strncmp(*env, "new_host_name=", 14) == 0) {
            host = *env + 14;
        } else if (strncmp(*env, "new_rtems_cmdline=", 18) == 0) {
            cmdline = *env + 18;
        }
    }

    if (reason != NULL && strcmp(reason, "BOUND") == 0) {
        if (host != NULL && host[0] != '\0') {
            sethostname(host, strlen(host));
        }
        /*
         * Refuse rather than truncate (base :789-791): half an address list is
         * a wrong address list, and it would be indistinguishable from one the
         * site actually asked for.
         */
        if (cmdline != NULL) {
            if (strlen(cmdline) >= sizeof(boot_cmdline)) {
                printf("rtems-boot: ***** DHCP rtems_cmdline is %zu bytes, "
                       "buffer is %zu; IGNORED. Expand boot_cmdline and "
                       "rebuild. *****\n",
                       strlen(cmdline), sizeof(boot_cmdline));
            } else {
                strcpy(boot_cmdline, cmdline);
            }
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

/*
 * Static network configuration (base #853: posix/rtems_init.c:1095-1131,
 * setBootConfigFromNVRAM.c `applyNetConfig`).
 *
 * Base discovers the addresses in motload/PPCBUG NVRAM at run time; this shim
 * has no NVRAM contract (§1.1 drops the NVRAM boot path), so the same values
 * arrive as compile-time defines instead. Build with
 *
 *     -DEPICS_RTEMS_STATIC_IP=\"10.0.0.5\"
 *     -DEPICS_RTEMS_STATIC_NETMASK=\"255.255.255.0\"
 *     -DEPICS_RTEMS_STATIC_GATEWAY=\"10.0.0.1\"   (optional)
 *
 * (via `CFLAGS_armv7-rtems-eabihf`; the `cc` crate forwards them) to configure
 * the first hardware interface statically. With the first two undefined the
 * image keeps its DHCP path, and — matching base, where a bad NVRAM config
 * returns -1 and `try_dhcp` takes over — a static setup that fails at run
 * time also falls back to DHCP rather than booting unreachable.
 */
#if defined(EPICS_RTEMS_STATIC_IP) && defined(EPICS_RTEMS_STATIC_NETMASK)

/*
 * Block until `ifname` reports link up via an RTM_IFINFO routing message, or
 * until `timeout_secs` elapses (base #853 `wait_for_link_up`, modeled there on
 * dhcpcd's `manage_link`). The static path needs this explicit wait because,
 * unlike DHCP — whose BOUND hook fires only once the link carries traffic —
 * `rtems_bsd_ifconfig` returns before the PHY negotiates.
 *
 * `route_sock` must already be open from before the interface was configured,
 * so no RTM_IFINFO event can be missed.
 */
static int wait_for_link_up(int route_sock, const char *ifname, int timeout_secs)
{
    struct timeval tv = {.tv_sec = timeout_secs, .tv_usec = 0};
    char buf[sizeof(struct if_msghdr) + sizeof(struct sockaddr_dl)];

    printf("rtems-boot: waiting for link on %s (timeout %d s)\n", ifname,
           timeout_secs);
    setsockopt(route_sock, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));

    while (recv(route_sock, buf, sizeof(buf), 0) > 0) {
        struct rt_msghdr *rtm = (struct rt_msghdr *)(void *)buf;
        if (rtm->rtm_type == RTM_IFINFO) {
            struct if_msghdr *ifm = (struct if_msghdr *)(void *)buf;
            char name[IFNAMSIZ];
            if (if_indextoname(ifm->ifm_index, name) != NULL &&
                strcmp(name, ifname) == 0 &&
                ifm->ifm_data.ifi_link_state == LINK_STATE_UP) {
                return 0;
            }
        }
    }

    /* recv returned <= 0: SO_RCVTIMEO expired (EAGAIN) or socket error. */
    printf("rtems-boot: ***** link did not come up on %s within %d s *****\n",
           ifname, timeout_secs);
    return -1;
}

/*
 * Configure the interface at index 1 — the index base's `applyNetConfig` and
 * `wait_for_link_up` both hard-code for "the first hardware interface" —
 * with the compiled-in address. Returns 1 when the interface is configured,
 * 0 to fall back to DHCP.
 */
static int configure_static_network(void)
{
    /* Writable copies: `rtems_bsd_ifconfig` takes `char *`. */
    static char ip[] = EPICS_RTEMS_STATIC_IP;
    static char netmask[] = EPICS_RTEMS_STATIC_NETMASK;
#ifdef EPICS_RTEMS_STATIC_GATEWAY
    static char gateway_buf[] = EPICS_RTEMS_STATIC_GATEWAY;
    char *gateway = gateway_buf;
#else
    char *gateway = NULL;
#endif
    char ifnamebuf[IF_NAMESIZE];
    char *ifname = if_indextoname(1, ifnamebuf);
    int route_sock;
    int exit_code;

    if (ifname == NULL) {
        printf("rtems-boot: no network interface found; trying DHCP\n");
        return 0;
    }

    /*
     * Open the route socket before configuring the interface, so the
     * RTM_IFINFO that reports link-up cannot be missed (base #853,
     * rtems_init.c:1102-1106).
     */
    route_sock = socket(PF_ROUTE, SOCK_RAW, 0);
    if (route_sock < 0) {
        printf("rtems-boot: cannot open PF_ROUTE socket: %s\n",
               strerror(errno));
    }

    printf("rtems-boot: static ifconfig %s ip=%s netmask=%s gateway=%s\n",
           ifname, ip, netmask, gateway != NULL ? gateway : "none");
    exit_code = rtems_bsd_ifconfig(ifname, ip, netmask, gateway);
    if (exit_code != EX_OK) {
        printf("rtems-boot: rtems_bsd_ifconfig failed (exit code %d); trying "
               "DHCP\n",
               exit_code);
        if (route_sock >= 0) {
            close(route_sock);
        }
        return 0;
    }

    /*
     * Block until the physical link comes up, analogous to the DHCP BOUND
     * wait; on timeout continue loudly, as the DHCP path does. Base's
     * timeout: 30 s (rtems_init.c:1148).
     */
    if (route_sock >= 0) {
        wait_for_link_up(route_sock, ifname, 30);
        close(route_sock);
    }
    return 1;
}

#else /* !EPICS_RTEMS_STATIC_IP || !EPICS_RTEMS_STATIC_NETMASK */

static int configure_static_network(void)
{
    return 0;
}

#endif

void *POSIX_Init(void *argument)
{
    struct timespec now;
    rtems_status_code sc;
    rtems_task_priority old_prio;
    int argc;
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
     * (`cpukit/posix/src/pthreadattrdefault.c:49-58` (both `rtems` pins)), and Rust's `std` never
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

    /*
     * 10. Network configuration (base #853, :1095-1160): static when the
     * image was built with an address — see configure_static_network above —
     * otherwise DHCP, bounded: boot anyway on timeout, loudly (base
     * :1050-1072).
     */
    if (!configure_static_network()) {
        rtems_dhcpcd_add_hook(&dhcpcd_hook);
        start_dhcpcd();
        for (waited = 0;
             !dhcp_bound && waited < EPICS_RTEMS_DHCP_TIMEOUT_SECONDS;
             ++waited) {
            rtems_task_wake_after(rtems_clock_get_ticks_per_second());
        }
        if (!dhcp_bound) {
            printf("rtems-boot: ***** DHCP did not bind in %d s; continuing "
                   "with no address. Server ports will bind but nothing "
                   "off-board can reach them. *****\n",
                   EPICS_RTEMS_DHCP_TIMEOUT_SECONDS);
        }
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

    /*
     * 11. The boot command line, then the Rust IOC (base :1127 then
     *     :1184-1191 — base builds argv from the BOOTP command line in
     *     `initialize_local_filesystem`/`initialize_remote_filesystem` and
     *     hands it to `main` the same way).
     *
     *     Built here, after DHCP, so the `rtems_cmdline` option has already
     *     had its chance to replace the compiled-in default; and echoed,
     *     because the console is this target's only report and a boot that
     *     silently took the default is the failure this closes.
     */
    argc = epics_rtems_build_boot_argv(
        boot_cmdline, boot_argv, (int)(sizeof(boot_argv) / sizeof(boot_argv[0])));
    {
        int i;
        printf("rtems-boot: boot command line (%d argument(s)):", argc - 1);
        for (i = 1; i < argc; ++i) {
            printf(" [%s]", boot_argv[i]);
        }
        printf("\n");
    }

    printf("rtems-boot: main() reached\n");
    result = main(argc, boot_argv);
    printf("rtems-boot: IOC terminated with %d\n", result);

    exit(result);
    delayedPanic("returned from exit()");
    return NULL;
}
