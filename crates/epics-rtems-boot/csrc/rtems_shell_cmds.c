/*
 * rtems_shell_cmds.c — the RTEMS-side half of the operator commands EPICS
 * base registers from `iocshRegisterRTEMS`
 * (`modules/libcom/RTEMS/posix/rtems_init.c:692-705` @R7.0.10, and its
 * `score/` twin at `:523-528`).
 *
 * Base registers six there — netstat, heapSpace, nfsMount, zoneset, rt,
 * setlogmask — and the Rust side of this port registers five of them from
 * `epics_base_rs::server::iocsh::register_rtems_commands`. This file is only
 * the part that has to call an RTEMS or libbsd API; everything expressible in
 * Rust (`zoneset`, the number formatting) stays in Rust, where it is host-
 * testable.
 *
 * Two of base's six are not here and each has its own reason:
 *
 *   * `heapSpace` reads the malloc heap, which is `rtems_stats.c`'s job — the
 *     one file in this crate that already owns `RTEMS_Malloc_Heap`. Its
 *     `epics_rtems_boot_heap_space` is base's exact three inputs.
 *   * `nfsMount` cannot be written at all on this stack. Base's own posix arm
 *     includes `<librtemsNfs.h>` only under `#ifdef RTEMS_LEGACY_STACK`
 *     (`rtems_init.c:60-62`) while calling `nfsMount()` under
 *     `#ifndef OMIT_NFS_SUPPORT` (`:593-604`, `:696`), so a base build on the
 *     libbsd stack that does NOT define `OMIT_NFS_SUPPORT` calls an undeclared
 *     function. The API itself is gone: rtems-libbsd's `librtemsNfs.h`
 *     declares `rpcUdpInit`, `nfsInit`, `nfsMountsShow` and
 *     `rtems_nfs_initialize` and no `nfsMount(char *, char *, char *)` (both
 *     libbsd pins). NFS on this stack is reached through `mount()` with
 *     `RTEMS_FILESYSTEM_TYPE_NFS`, which is a different command, and this
 *     image configures no NFS filesystem. So the command is absent here
 *     because it is absent on the target, not because it was skipped.
 *
 * NOT BUILT FOR THE TARGET ON THIS MACHINE. Like `rtems_init.c` it is
 * compiled FOR THE HOST on every push against the RTEMS declarations recorded
 * in tests/rtems-api/, so a name, an arity or a format string that is wrong
 * here fails CI rather than the board's console. That record's README.md says
 * what the host compile does and does not prove.
 */

/* Base does the same, for the same reason: it is what makes <syslog.h> emit
 * the prioritynames[] table the level listing walks (base :21-22). */
#define SYSLOG_NAMES

#include <stdio.h>
#include <string.h>
#include <syslog.h>

#include <rtems/bsd/bsd.h>
#include <rtems/shell.h>

#include <machine/rtems-bsd-commands.h>

/*
 * `netstat <level>` — base `netStatCallFunc`/`rtems_netstat` (:531-547).
 *
 * DELIBERATE DEVIATION, and the only one in this file: base's own body on
 * this stack is
 *
 *     printf("***** Sorry not implemented yet with the new network stack
 *            (bsdlib)\n");
 *
 * because `rtems_netstat` is written against the legacy `rtems_bsdnet_show_*`
 * calls and everything but the apology is inside `#ifdef RTEMS_LEGACY_STACK`.
 * An IOC on RTEMS 6/7 therefore has a `netstat` command that reports nothing.
 *
 * The readings it wanted are all still available on libbsd, from the same
 * `netstat` this crate's `POSIX_Init` already runs at boot
 * (`rtems_init.c:508-516`), so this produces them instead of the apology.
 * The mapping is base's own level ladder, one flag per legacy call:
 *
 *     level >= 0   rtems_bsdnet_show_if_stats     -> netstat -i
 *                  rtems_bsdnet_show_mbuf_stats   -> netstat -m
 *     level >= 1   rtems_bsdnet_show_inet_routes  -> netstat -rn
 *     level >= 2   show_{ip,icmp,udp,tcp}_stats   -> netstat -s
 *
 * `-s` is one call where base made four; libbsd's netstat has no per-protocol
 * flag and prints all four sections, which is the same information in one
 * pass rather than a subset.
 *
 * Void, like base's: `netStatCallFunc` (:548-551) has no failure path and
 * `rtems_netstat` returns nothing, so a `netstat` line cannot fail a script
 * here either. libbsd's netstat reports its own errors on the console.
 */
void epics_rtems_boot_netstat(int level) {
  static char netstat[] = "netstat";
  static char dash_i[] = "-i";
  static char dash_m[] = "-m";
  static char dash_rn[] = "-rn";
  static char dash_s[] = "-s";

  char *if_argv[] = {netstat, dash_i, NULL};
  char *mbuf_argv[] = {netstat, dash_m, NULL};
  char *route_argv[] = {netstat, dash_rn, NULL};
  char *proto_argv[] = {netstat, dash_s, NULL};

  /* Base flushes around every shell hand-off (`rtshellCallFunc`, :512-519)
   * because the C library's buffer and the command's own output otherwise
   * interleave on one serial line. */
  fflush(stdout);

  (void)rtems_bsd_command_netstat(2, if_argv);
  (void)rtems_bsd_command_netstat(2, mbuf_argv);

  if (level >= 1) {
    (void)rtems_bsd_command_netstat(2, route_argv);
  }

  if (level >= 2) {
    (void)rtems_bsd_command_netstat(2, proto_argv);
  }

  fflush(stdout);
}

/*
 * `rt <cmd> <args...>` — base `rtshellCallFunc` (:506-524).
 *
 * `rtems_shell_init_environment` is what links the configured command set
 * (`rtems_config.c` §L) into the list `rtems_shell_lookup_cmd` walks. Base
 * calls it once, from `iocshRegisterRTEMS` itself (:704). It is called here
 * instead, on every lookup, so the ordering invariant holds BY CONSTRUCTION —
 * there is no way to reach a lookup without it — rather than by remembering
 * to register in the right order from Rust. It is `pthread_once`-guarded
 * upstream (`cpukit/libmisc/shell/shell.c:203-206`), so the repeat costs one
 * atomic.
 *
 * Returns 0 with `*status` set when the command ran, -1 when there is no such
 * command. `*status` is untouched in the second case, so a caller cannot read
 * a stale exit code as a real one.
 */
int epics_rtems_boot_shell_run(const char *cmd, int argc, char **argv,
                               int *status) {
  rtems_shell_cmd_t *found;

  if (cmd == NULL || argv == NULL || status == NULL) {
    return -1;
  }

  rtems_shell_init_environment();

  found = rtems_shell_lookup_cmd(cmd);
  if (found == NULL) {
    return -1;
  }

  fflush(stdout);
  fflush(stderr);
  *status = (*found->command)(argc, argv);
  fflush(stdout);
  fflush(stderr);
  return 0;
}

/*
 * `setlogmask <level>` — base `setlogmaskCallFunc` (:660-686).
 *
 * Base does two things with the level it matched: `setlogmask(LOG_MASK(val))`
 * and, on the libbsd stack only, `rtems_bsd_setlogpriority(name)`. Only the
 * second has an effect here, and that is libbsd's own decision rather than an
 * inference: its `setlogmask` is a stub that ignores its argument and returns
 * 0, with the comment that the mask is process-wide, that RTEMS has no
 * processes, and that "the syslog mask can be set via
 * rtems_bsd_setlogpriority()" (`rtemsbsd/rtems/syslog.c:92-104`, both libbsd
 * pins). Calling it as well would be implementation parity with a provably
 * inert call, so this does not.
 *
 * The name lookup stays base's — `strcmp` against `prioritynames`, so an
 * unknown level is refused here rather than by libbsd, and the case-
 * insensitive match `rtems_bsd_setlogpriority` would do on its own is not
 * silently more permissive than a C IOC.
 *
 * Note the two spellings differ in reach: base asks for `LOG_MASK(val)` — one
 * level — while libbsd sets `LOG_UPTO(c_val)`, that level and everything more
 * severe. That is the behaviour a base IOC on this stack already has, since
 * `rtems_bsd_setlogpriority` is the call that lands.
 *
 * Returns 0 when the level was known and set, -1 otherwise.
 */
int epics_rtems_boot_set_log_priority(const char *name) {
  const CODE *cur;

  if (name == NULL) {
    return -1;
  }

  for (cur = prioritynames; cur->c_name; cur++) {
    if (strcmp(name, cur->c_name) != 0) {
      continue;
    }
    return rtems_bsd_setlogpriority(name) == 0 ? 0 : -1;
  }

  return -1;
}

/*
 * The syslog level names, by index, NULL past the end — base's usage listing
 * (:665-671) walked from the caller's side.
 *
 * An index rather than the table pointer because `prioritynames` is a
 * definition inside <syslog.h>, so its element type is the C library's, not
 * one this file could name in a header for Rust to match.
 */
const char *epics_rtems_boot_log_priority_name(unsigned index) {
  unsigned i;

  for (i = 0; prioritynames[i].c_name; i++) {
    if (i == index) {
      return prioritynames[i].c_name;
    }
  }

  return NULL;
}
