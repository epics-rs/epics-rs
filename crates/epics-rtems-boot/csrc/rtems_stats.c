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

/*
 * STAGE-5 PROBE — MEASUREMENT ONLY (doc/pvalink-rtems-design.md §5 stage 5,
 * pass criterion 6). Not merged; see doc/pvalink-stage5-probe.patch.
 *
 * Criterion 6 asks for `rt stackuse` / `rt top` on the guest. This image
 * configures the shell's commands (CONFIGURE_SHELL_COMMAND_STACKUSE,
 * rtems_config.c) but never starts a shell task, and the target has no
 * console input path at all — so the two readings have to be produced from
 * inside the image:
 *
 *   * `epics_rtems_boot_stack_report` calls the exact function the shell's
 *     `stackuse` command calls (`rtems_stack_checker_report_usage`,
 *     cpukit/libmisc/stackchk/check.c), which is why the output format below
 *     is the shell's own, not something this file invents. It needs
 *     CONFIGURE_STACK_CHECKER_ENABLED, which rtems_config.c:223 already sets.
 *   * `epics_rtems_boot_dump_tasks` is the task census (the `rt top` half —
 *     thread count, kernel names, effective priorities). It is verbatim the
 *     probe doc/rtems-priority-probe.patch used for the priority measurement,
 *     reused here rather than re-invented so both measurements are reading
 *     the same listing.
 *
 * The visitor only copies ids: `rtems_task_iterate` runs it with the object
 * allocator mutex held, so every query that could block or take another lock
 * (`rtems_object_get_name`, `rtems_task_get_priority`) is done afterwards.
 */
#include <stdio.h>
#include <string.h>

#include <rtems/stackchk.h>
#include <rtems/score/objectdata.h>
#include <rtems/score/thread.h>
#include <rtems/score/threadimpl.h>

#define EPICS_RTEMS_DUMP_MAX_TASKS 192

static rtems_id epics_rtems_dump_ids[EPICS_RTEMS_DUMP_MAX_TASKS];
/* Two names per task, because they are two different facts. `object` is what
 * `rtems_object_get_name` reports — the classic Objects_Name, which a POSIX
 * thread never has. `thread` is `tcb->name`, which is what
 * `pthread_setname_np` writes. */
static char epics_rtems_dump_object_names[EPICS_RTEMS_DUMP_MAX_TASKS][32];
static char epics_rtems_dump_thread_names[EPICS_RTEMS_DUMP_MAX_TASKS][32];
static uint32_t epics_rtems_dump_count;

static bool epics_rtems_dump_collect(rtems_tcb *tcb, void *arg) {
  (void)arg;
  if (epics_rtems_dump_count < EPICS_RTEMS_DUMP_MAX_TASKS) {
    uint32_t i = epics_rtems_dump_count++;
    epics_rtems_dump_ids[i] = tcb->Object.id;
    /* A memcpy out of the TCB, no lock taken — safe inside the visitor. */
    memset(epics_rtems_dump_thread_names[i], 0,
           sizeof(epics_rtems_dump_thread_names[i]));
    _Thread_Get_name(tcb, epics_rtems_dump_thread_names[i],
                     sizeof(epics_rtems_dump_thread_names[i]));
  }
  return false; /* false == keep iterating */
}

void epics_rtems_boot_dump_tasks(const char *tag) {
  uint32_t i;
  rtems_id scheduler = 0;
  rtems_status_code sc;

  epics_rtems_dump_count = 0;
  rtems_task_iterate(epics_rtems_dump_collect, NULL);

  sc = rtems_task_get_scheduler(RTEMS_SELF, &scheduler);
  printf("TASKDUMP begin tag=%s count=%lu scheduler_sc=%d\n",
         tag == NULL ? "?" : tag, (unsigned long)epics_rtems_dump_count,
         (int)sc);

  for (i = 0; i < epics_rtems_dump_count; i++) {
    rtems_task_priority prio = 0;
    rtems_status_code psc;
    char *obj;

    memset(epics_rtems_dump_object_names[i], 0,
           sizeof(epics_rtems_dump_object_names[i]));
    obj = rtems_object_get_name(epics_rtems_dump_ids[i],
                                sizeof(epics_rtems_dump_object_names[i]),
                                epics_rtems_dump_object_names[i]);
    if (obj == NULL) {
      strcpy(epics_rtems_dump_object_names[i], "-");
    }
    psc = rtems_task_get_priority(epics_rtems_dump_ids[i], scheduler, &prio);
    printf("TASKDUMP id=0x%08lx core=%3lu posix=%4ld sc=%d obj=%-6s thread=%s\n",
           (unsigned long)epics_rtems_dump_ids[i], (unsigned long)prio,
           (long)255 - (long)prio, (int)psc, epics_rtems_dump_object_names[i],
           epics_rtems_dump_thread_names[i][0] == '\0'
               ? "<empty>"
               : epics_rtems_dump_thread_names[i]);
  }
  printf("TASKDUMP end tag=%s\n", tag == NULL ? "?" : tag);
  fflush(stdout);
}

/* `rt stackuse`: the shell command's own implementation, called directly. */
void epics_rtems_boot_stack_report(const char *tag) {
  printf("STACKUSE begin tag=%s\n", tag == NULL ? "?" : tag);
  fflush(stdout);
  rtems_stack_checker_report_usage();
  printf("STACKUSE end tag=%s\n", tag == NULL ? "?" : tag);
  fflush(stdout);
}

/*
 * BRING-UP PROBE — the descriptor census behind `epics_rtems_boot_fd_usage`.
 *
 * `fd_usage` answers *how many* descriptors are open, which is the number that
 * predicts the connection ceiling. It cannot answer *which*, and an outage
 * measurement needs exactly that: a client whose circuits are all down still
 * holds one descriptor more than it held at boot, and "one unexplained fd" is
 * only a finding once the other seven are named.
 *
 * The walk is the same one `epics_rtems_boot_fd_usage` does — the same table,
 * the same LIBIO_FLAGS_OPEN test — so the census cannot disagree with the count
 * beside it. Each open descriptor is then classified through POSIX rather than
 * through libio internals:
 *
 *   * `SO_TYPE` succeeds only on a socket, and names it TCP or UDP.
 *   * `SO_ACCEPTCONN` separates a listening socket from a connected one
 *     without inferring it from a `getpeername` failure.
 *   * `getsockname`/`getpeername` give the addresses, formatted from the
 *     `sockaddr_in` bytes here rather than through `inet_ntop`, so the
 *     printout does not depend on the length-byte handling that has already
 *     bitten this target once.
 *   * anything that is not a socket gets `fstat`'s mode, which is what
 *     distinguishes the console from a file.
 *
 * Read-only on every descriptor it touches: it can run while the pumps own
 * their sockets.
 */
#include <errno.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/stat.h>

static void epics_rtems_fmt_sockaddr(const struct sockaddr_storage *ss,
                                     char *out, size_t outlen) {
  if (ss->ss_family == AF_INET) {
    const struct sockaddr_in *sin = (const struct sockaddr_in *)ss;
    const unsigned char *b = (const unsigned char *)&sin->sin_addr.s_addr;
    snprintf(out, outlen, "%u.%u.%u.%u:%u", (unsigned)b[0], (unsigned)b[1],
             (unsigned)b[2], (unsigned)b[3], (unsigned)ntohs(sin->sin_port));
  } else {
    snprintf(out, outlen, "family=%u", (unsigned)ss->ss_family);
  }
}

void epics_rtems_boot_fd_census(const char *tag) {
  uint32_t i;
  uint32_t open_count = 0;
  const char *t = tag == NULL ? "?" : tag;

  printf("FDCENSUS begin tag=%s\n", t);
  for (i = 0; i < rtems_libio_number_iops; i++) {
    int fd;
    int type = 0;
    int listening = 0;
    socklen_t len;
    char local[48];
    char peer[48];
    struct sockaddr_storage addr;
    struct stat st;

    if ((rtems_libio_iop_flags(&rtems_libio_iops[i]) & LIBIO_FLAGS_OPEN) == 0) {
      continue;
    }
    fd = (int)i;
    open_count++;

    len = (socklen_t)sizeof(type);
    if (getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &len) != 0) {
      int sockerr = errno;
      if (fstat(fd, &st) == 0) {
        printf("FDCENSUS tag=%s fd=%d kind=nonsocket mode=0%o so_type_errno=%d\n",
               t, fd, (unsigned)st.st_mode, sockerr);
      } else {
        printf("FDCENSUS tag=%s fd=%d kind=unknown so_type_errno=%d "
               "fstat_errno=%d\n",
               t, fd, sockerr, errno);
      }
      continue;
    }

    len = (socklen_t)sizeof(listening);
    if (getsockopt(fd, SOL_SOCKET, SO_ACCEPTCONN, &listening, &len) != 0) {
      listening = -1;
    }

    strcpy(local, "-");
    strcpy(peer, "-");
    len = (socklen_t)sizeof(addr);
    memset(&addr, 0, sizeof(addr));
    if (getsockname(fd, (struct sockaddr *)&addr, &len) == 0) {
      epics_rtems_fmt_sockaddr(&addr, local, sizeof(local));
    }
    len = (socklen_t)sizeof(addr);
    memset(&addr, 0, sizeof(addr));
    if (getpeername(fd, (struct sockaddr *)&addr, &len) == 0) {
      epics_rtems_fmt_sockaddr(&addr, peer, sizeof(peer));
    } else {
      snprintf(peer, sizeof(peer), "none(errno=%d)", errno);
    }

    printf("FDCENSUS tag=%s fd=%d kind=%s listening=%d local=%s peer=%s\n", t,
           fd,
           type == SOCK_STREAM  ? "tcp"
           : type == SOCK_DGRAM ? "udp"
                                : "socket",
           listening, local, peer);
  }
  printf("FDCENSUS end tag=%s open=%lu max=%lu\n", t,
         (unsigned long)open_count, (unsigned long)rtems_libio_number_iops);
  fflush(stdout);
}
