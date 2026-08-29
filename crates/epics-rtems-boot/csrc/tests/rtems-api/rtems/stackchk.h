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

#ifndef EPICS_RS_RECORDED_RTEMS_STACKCHK_H
#define EPICS_RS_RECORDED_RTEMS_STACKCHK_H

#include <rtems/printer.h>

/* @rtems-api rtems/stackchk.h */
void rtems_stack_checker_report_usage_with_plugin(
  const rtems_printer *printer
);
/* @rtems-api-end */

#endif /* EPICS_RS_RECORDED_RTEMS_STACKCHK_H */
