/*
 * Recorded RTEMS 6 / rtems-libbsd declarations — the subset that
 * csrc/rtems_shell_cmds.c names, and nothing else.
 *
 * This is NOT a copy of the real header. Every block introduced by an
 * `@rtems-api <header>` marker is verbatim text from that header in an
 * installed RTEMS 6 BSP, running to its `@rtems-api-end`, and
 * `scripts/rtems-api-check.sh` proves it line for line against a real
 * $RTEMS_BSP_PREFIX. Anything introduced by `@rtems-api-local` is ours and
 * carries its own justification.
 *
 * The recorded text stays under its authors' licences: RTEMS (BSD-2-Clause,
 * (c) OAR Corporation and contributors) and FreeBSD via rtems-libbsd
 * (BSD-3-Clause, (c) The Regents of the University of California and
 * contributors).
 *
 * Every block below is byte-identical in both series this workspace builds
 * against — kernel `181e86a199` (series 7) and `51f962fb3f` (series 6),
 * the pins `scripts/rtems-bsp.sh` installs — so pass 2 holds against either
 * prefix. That is checked, not assumed: the two headers disagree elsewhere
 * (the whole file is offset by 18 lines), which is why the record carries
 * declarations rather than line numbers.
 *
 * See tests/rtems-api/README.md for why this exists.
 */

#ifndef EPICS_RS_RECORDED_RTEMS_SHELL_H
#define EPICS_RS_RECORDED_RTEMS_SHELL_H

/* @rtems-api-local: the recorded struct names mode_t, uid_t and gid_t, which
 * the real header gets from its own <sys/types.h> include chain and the host
 * supplies identically. */
#include <sys/types.h>

/* @rtems-api rtems/shell.h */
typedef int (*rtems_shell_command_t)(int argc, char **argv);
/* @rtems-api-end */

/* @rtems-api rtems/shell.h */
struct rtems_shell_cmd_tt;
/* @rtems-api-end */

/* @rtems-api rtems/shell.h */
typedef struct rtems_shell_cmd_tt rtems_shell_cmd_t;
/* @rtems-api-end */

/* @rtems-api rtems/shell.h */
struct rtems_shell_cmd_tt {
  const char            *name;
  const char            *usage;
  const char            *topic;
  rtems_shell_command_t  command;
  rtems_shell_cmd_t     *alias;
  rtems_shell_cmd_t     *next;
  mode_t                 mode;
  uid_t                  uid;
  gid_t                  gid;
};
/* @rtems-api-end */

/* @rtems-api rtems/shell.h */
extern rtems_shell_cmd_t * rtems_shell_lookup_cmd(const char *cmd);
/* @rtems-api-end */

/* @rtems-api rtems/shell.h */
extern void rtems_shell_init_environment(
  void
);
/* @rtems-api-end */

#endif /* EPICS_RS_RECORDED_RTEMS_SHELL_H */
