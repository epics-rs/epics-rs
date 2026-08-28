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

#ifndef EPICS_RS_RECORDED_RTEMS_H
#define EPICS_RS_RECORDED_RTEMS_H

/* @rtems-api-local: the recorded text needs fixed-width integers and the
 * host's <stdint.h> supplies them exactly as newlib's does. */
#include <stdint.h>

/* @rtems-api-local: basedefs.h defines these two by dialect, one arm per
 * #if. Recorded here is the arm the compiler actually takes for this file:
 * both the host gate and the image build are -std=gnu17, so __STDC_VERSION__
 * is 201710L and RTEMS_NO_RETURN is _Noreturn; __GNUC__ is defined either
 * way, and the empty RTEMS_PRINTFLIKE arm exists only for a non-GNU
 * compiler, which nothing here is. */

/* @rtems-api rtems/score/basedefs.h */
  #define RTEMS_NO_RETURN _Noreturn
/* @rtems-api-end */

/* @rtems-api rtems/score/basedefs.h */
  #define RTEMS_PRINTFLIKE( _format_pos, _ap_pos ) \
    __attribute__(( __format__( __printf__, _format_pos, _ap_pos ) ))
/* @rtems-api-end */

/* @rtems-api rtems/score/object.h */
typedef uint32_t   Objects_Id;
/* @rtems-api-end */

/* @rtems-api rtems/score/object.h */
#define OBJECTS_ID_OF_SELF ((Objects_Id) 0)
/* @rtems-api-end */

/* @rtems-api rtems/score/watchdogticks.h */
typedef uint32_t   Watchdog_Interval;
/* @rtems-api-end */

/* @rtems-api rtems/score/watchdogticks.h */
extern const uint32_t _Watchdog_Ticks_per_second;
/* @rtems-api-end */

/* @rtems-api rtems/rtems/types.h */
typedef Objects_Id rtems_id;
/* @rtems-api-end */

/* @rtems-api rtems/rtems/types.h */
typedef Watchdog_Interval rtems_interval;
/* @rtems-api-end */

/* @rtems-api rtems/rtems/types.h */
typedef uint32_t rtems_task_priority;
/* @rtems-api-end */

/* @rtems-api rtems/rtems/status.h */
typedef enum {
/* @rtems-api-end */

/* @rtems-api rtems/rtems/status.h */
  RTEMS_SUCCESSFUL = 0,
/* @rtems-api-end */

/* @rtems-api-local: the enum has 30 enumerators and the other 29 are elided.
 * rtems_init.c names only RTEMS_SUCCESSFUL, every other enumerator is
 * non-zero, and the file's only use of the type is `sc != RTEMS_SUCCESSFUL`.
 * Widening it would be recorded the same way as anything else. */

/* @rtems-api rtems/rtems/status.h */
} rtems_status_code;
/* @rtems-api-end */

/* @rtems-api rtems/rtems/tasks.h */
rtems_task_priority _RTEMS_Maximum_priority( void );
/* @rtems-api-end */

/* @rtems-api rtems/rtems/tasks.h */
#define RTEMS_MAXIMUM_PRIORITY _RTEMS_Maximum_priority()
/* @rtems-api-end */

/* @rtems-api rtems/rtems/tasks.h */
#define RTEMS_SELF OBJECTS_ID_OF_SELF
/* @rtems-api-end */

/* @rtems-api rtems/rtems/tasks.h */
rtems_status_code rtems_task_set_priority(
  rtems_id             id,
  rtems_task_priority  new_priority,
  rtems_task_priority *old_priority
);
/* @rtems-api-end */

/* @rtems-api rtems/rtems/tasks.h */
rtems_status_code rtems_task_wake_after( rtems_interval ticks );
/* @rtems-api-end */

/* @rtems-api rtems/rtems/clock.h */
rtems_interval rtems_clock_get_ticks_per_second( void );
/* @rtems-api-end */

/* @rtems-api rtems/rtems/clock.h */
#define rtems_clock_get_ticks_per_second() _Watchdog_Ticks_per_second
/* @rtems-api-end */

/* @rtems-api rtems/config.h */
const char *rtems_get_version_string( void );
/* @rtems-api-end */

/* @rtems-api rtems/fatal.h */
RTEMS_NO_RETURN RTEMS_PRINTFLIKE( 1, 2 ) void rtems_panic(
  const char *fmt,
  ...
);
/* @rtems-api-end */

#endif /* EPICS_RS_RECORDED_RTEMS_H */
