/*
 * rtems_stats.c — the two IOC-statistics values that need kernel visibility.
 *
 * Descriptor usage and heap usage are the two numbers on this target that Rust
 * cannot reach: `std` exposes neither, and both live behind RTEMS internals
 * (`rtems_libio_iops`, `RTEMS_Malloc_Heap`) rather than behind POSIX. Every
 * other status value the IOC publishes is a Rust counter.
 *
 * Ported from devIocStats' RTEMS OSD arm — `devIocStats/os/RTEMS/osdFdUsage.c`
 * and `osdMemUsage.c` in <https://github.com/epics-modules/iocStats> — which
 * is where the choice of source of truth comes from. Two deviations from that
 * code, both forced by RTEMS 6 and both measured against this BSP's installed
 * headers rather than assumed:
 *
 *   * `iop->flags` is `Atomic_Uint` in RTEMS 6 (`rtems/libio.h:1403-1406`), so
 *     upstream's `rtems_libio_iops[i].flags & LIBIO_FLAGS_OPEN` does not
 *     compile here. The public inline accessor `rtems_libio_iop_flags()` is
 *     the RTEMS 6 spelling and is what this uses.
 *   * Upstream's `#ifdef RTEMS_PROTECTED_HEAP` / `rtems_region_get_information`
 *     fallback is for RTEMS <= 4.7. RTEMS 6 always has the protected heap, so
 *     the fallback arm is dropped rather than carried as dead code.
 *
 * The heap walk is not cheap — upstream says so in its own header comment
 * ("Gathering heap statistics could be expensive; I wouldn't want to run this
 * too often w/o knowing how it is implemented") — which is a constraint on the
 * caller's polling rate, not on this file. The IOC's status pusher runs at one
 * second (`epics_base_rs::server::status_pv::PUSH_INTERVAL`).
 *
 * Both functions return 0 on success and -1 on failure, and write nothing on
 * failure, so a caller can tell "no value" from "the value is zero" — which
 * matters because zero free descriptors and zero free heap are both real
 * readings.
 */

#include <stddef.h>
#include <stdint.h>

#include <rtems.h>
#include <rtems/libio.h>
/* Internal header: `rtems_libio_iops` and `rtems_libio_number_iops` are
 * declared here (`rtems/libio_.h:80-81`) and nowhere public. devIocStats
 * reaches for the same header for the same reason. */
#include <rtems/libio_.h>
#include <rtems/malloc.h>
#include <rtems/score/protectedheap.h>

/*
 * Open descriptors, and the ceiling they are counted against.
 *
 * `rtems_libio_number_iops` is the configured descriptor table size —
 * `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS` in rtems_config.c — and it is the limit
 * the bring-up box hit first: at 142 concurrent CA connections the 143rd was
 * refused by the libbsd socket zone, derived from this cap. So this pair is
 * the most predictive thing the IOC can publish about how close it is to
 * refusing clients — and MEASURED, it is the *only* one that sees this wall:
 * CA_REFUSED_CNT stayed 0 through the whole ramp, because an accept-time
 * ENFILE happens before a client object exists. Cap, walls and the operator
 * consequences: doc/rtems-fd-ceiling-deviation.md.
 */
int epics_rtems_boot_fd_usage(uint32_t *used, uint32_t *max) {
  uint32_t i;
  uint32_t open_count = 0;

  if (used == NULL || max == NULL) {
    return -1;
  }

  for (i = 0; i < rtems_libio_number_iops; i++) {
    if ((rtems_libio_iop_flags(&rtems_libio_iops[i]) & LIBIO_FLAGS_OPEN) != 0) {
      open_count++;
    }
  }

  *used = open_count;
  *max = (uint32_t)rtems_libio_number_iops;
  return 0;
}

/*
 * Malloc-heap usage.
 *
 * `free_largest` is reported separately from `free_total` on purpose: it is
 * the fragmentation signal. Upstream's header comment makes the point that
 * vxStats assumed `total = free + used` and that the difference between that
 * and the true total is the hint about fragmentation; RTEMS gives us the
 * largest free block directly, which is the sharper form of the same signal —
 * an allocation fails on `largest`, not on `total`.
 *
 * `total` is deliberately NOT reported here. Upstream computes it as
 * `Free.total + Used.total` (osdMemUsage.c:73), which is a derived number, not
 * a new measurement; the caller can add. Returning only what the kernel
 * measured keeps this file free of a value that could drift from its parts.
 *
 * RTEMS's workspace area is a separate allocator and is not counted; upstream
 * puts it in its own OSD file (`osdWorkspaceUsage.c`) for the same reason.
 */
int epics_rtems_boot_mem_usage(uint64_t *free_total, uint64_t *used_total,
                               uint64_t *free_largest) {
  Heap_Information_block info;

  if (free_total == NULL || used_total == NULL || free_largest == NULL) {
    return -1;
  }

  if (!_Protected_heap_Get_information(RTEMS_Malloc_Heap, &info)) {
    return -1;
  }

  *free_total = (uint64_t)info.Free.total;
  *used_total = (uint64_t)info.Used.total;
  *free_largest = (uint64_t)info.Free.largest;
  return 0;
}
