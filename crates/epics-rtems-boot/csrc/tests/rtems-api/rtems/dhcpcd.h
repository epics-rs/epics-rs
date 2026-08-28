/*
 * Recorded RTEMS 6 / rtems-libbsd declarations — the subset that
 * csrc/rtems_init.c names, and nothing else.
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

#ifndef EPICS_RS_RECORDED_RTEMS_DHCPCD_H
#define EPICS_RS_RECORDED_RTEMS_DHCPCD_H

/* @rtems-api-local: rtems_dhcpcd_hook chains itself with SLIST_ENTRY, which
 * the real header pulls from <sys/queue.h> the same way. */
#include <sys/queue.h>

#include <rtems.h>

/* @rtems-api rtems/dhcpcd.h */
typedef struct rtems_dhcpcd_config {
	rtems_task_priority priority;
	int argc;
	char **argv;
	void (*prepare)(const struct rtems_dhcpcd_config *config,
	    int argc, char **argv);
	void (*destroy)(const struct rtems_dhcpcd_config *config,
	    int exit_code);
} rtems_dhcpcd_config;
/* @rtems-api-end */

/* @rtems-api rtems/dhcpcd.h */
rtems_status_code rtems_dhcpcd_start(const rtems_dhcpcd_config *config);
/* @rtems-api-end */

/* @rtems-api rtems/dhcpcd.h */
typedef struct rtems_dhcpcd_hook {
	SLIST_ENTRY(rtems_dhcpcd_hook) node;
	const char *name;
	void (*handler)(struct rtems_dhcpcd_hook *hook, char *const *env);
} rtems_dhcpcd_hook;
/* @rtems-api-end */

/* @rtems-api rtems/dhcpcd.h */
void rtems_dhcpcd_add_hook(rtems_dhcpcd_hook *hook);
/* @rtems-api-end */

#endif /* EPICS_RS_RECORDED_RTEMS_DHCPCD_H */
