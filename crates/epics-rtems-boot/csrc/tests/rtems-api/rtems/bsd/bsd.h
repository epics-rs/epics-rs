/*
 * Recorded RTEMS 6 / rtems-libbsd declarations — the subset that
 * csrc/rtems_init.c and csrc/rtems_shell_cmds.c name, and nothing else.
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
 * See tests/rtems-api/README.md for why this exists.
 */

#ifndef EPICS_RS_RECORDED_RTEMS_BSD_BSD_H
#define EPICS_RS_RECORDED_RTEMS_BSD_BSD_H

#include <rtems.h>

/* @rtems-api rtems/bsd/bsd.h */
rtems_status_code rtems_bsd_initialize(void);
/* @rtems-api-end */

/* @rtems-api rtems/bsd/bsd.h */
int rtems_bsd_ifconfig_lo0(void);
/* @rtems-api-end */

/* @rtems-api rtems/bsd/bsd.h */
int rtems_bsd_ifconfig(const char *ifname, const char *addr_self,
    const char *netmask, const char *addr_gateway);
/* @rtems-api-end */

/* @rtems-api rtems/bsd/bsd.h */
int rtems_bsd_setlogpriority(const char* priority);
/* @rtems-api-end */

#endif /* EPICS_RS_RECORDED_RTEMS_BSD_BSD_H */
