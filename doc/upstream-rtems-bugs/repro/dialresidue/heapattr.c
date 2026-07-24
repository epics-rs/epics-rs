/*
 * heapattr.c -- MEASUREMENT-ONLY heap attribution shim for the RTEMS images.
 *
 * NOT part of the shipped boot shim.  This file exists to answer one question
 * that a `#[global_allocator]` counter cannot: *which call site* still owns the
 * bytes that survive a connection attempt.  A global allocator counter on this
 * target reads 0 (std's allocations go straight to libc `malloc`, not through
 * the Rust global-allocator hook the counter wraps), so the accounting is done
 * at the C level, where every allocation on the image really lands.
 *
 * How it works
 * ------------
 *   -Wl,--wrap=malloc,--wrap=free,--wrap=posix_memalign,--wrap=aligned_alloc
 * makes every reference the linker resolves land in `__wrap_*` here; the real
 * allocator is reachable as `__real_*`.  Each live block is recorded in an
 * open-addressed pointer table together with its requested size and a *site
 * id*.  Frees remove it.  Two incremental indexes are kept so a report never
 * has to walk the big table:
 *
 *   size_bucket[]  live count / live bytes per requested size
 *   site[]         live count / live bytes / total count per call site
 *
 * The site id is a conservative stack signature, not a frame-pointer walk.
 * rustc emits A32 with LLVM's frame layout and the C shim is built `-mthumb`
 * with gcc's; the two conventions disagree, so `__builtin_return_address(n>0)`
 * is not trustworthy across that boundary.  Instead the wrapper scans words
 * upward from its own frame -- that region is live caller frames, so the first
 * words that fall inside the image's .text are the real return addresses --
 * and keeps the first SITE_PCS of them.  Only `__real_*`'s own immediate
 * caller would be reachable by `__builtin_return_address(0)`, and that is
 * always the same allocator shim, which is exactly the useless answer.
 *
 * Mutual exclusion is interrupt-off, not a spinlock and not a pthread mutex: a
 * spinlock deadlocks a uniprocessor under SCHED_FIFO when a high-priority
 * thread preempts the holder, and a pthread mutex would allocate on first use
 * from inside the allocator.  The critical sections are a few dozen
 * instructions.
 */

#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

void *__real_malloc(size_t);
void __real_free(void *);
int __real_posix_memalign(void **, size_t, size_t);
void *__real_aligned_alloc(size_t, size_t);

/* The image's own text bounds, so a stack word can be classified as a return
 * address.  These are BSP linker symbols (bsp/linker-symbols.h). */
extern char bsp_section_text_begin[];
extern char bsp_section_text_end[];

#define TBL_BITS 17
#define TBL_SIZE (1u << TBL_BITS)
#define TBL_MASK (TBL_SIZE - 1u)

#define SIZE_SLOTS 2048u
#define SIZE_MASK (SIZE_SLOTS - 1u)

#define SITE_SLOTS 16384u
#define SITE_PCS 6u

#define SCAN_WORDS 192u

/* Live bytes a site must hold before the full listing prints it.  A ~40 B/attempt
 * cost over a 240-attempt run holds ~9.6 kB, far above this. */
#define SITE_REPORT_MIN 8u

/* Slot state: NULL = never used (a probe run ends here), TOMBSTONE = freed
 * (a probe run continues past it), anything else = a live block. */
#define TOMBSTONE ((void *)1)

typedef struct {
    void *p;
    uint32_t size;
    uint32_t site;
} ent_t;

typedef struct {
    uint32_t size;   /* requested size; 0xffffffff = empty */
    uint32_t live;
    uint32_t bytes;
} sizerec_t;

typedef struct {
    uint32_t pc[SITE_PCS];
    uint32_t live;
    uint32_t bytes;
    uint32_t total;
    uint32_t used;
} siterec_t;

static ent_t tbl[TBL_SIZE];
static sizerec_t sizes[SIZE_SLOTS];
static siterec_t sites[SITE_SLOTS];

static uint32_t stat_alloc, stat_free, stat_untracked_free;
static uint32_t stat_tbl_overflow, stat_site_overflow, stat_size_overflow;
static uint32_t live_blocks, live_bytes;
static uint32_t tbl_used;
static int inited;

static inline uint32_t irq_off(void)
{
    uint32_t c;
    __asm__ volatile("mrs %0, cpsr\n\tcpsid i" : "=r"(c) : : "memory");
    return c;
}

static inline void irq_on(uint32_t c)
{
    __asm__ volatile("msr cpsr_c, %0" : : "r"(c) : "memory");
}

static void heapattr_init(void)
{
    unsigned i;
    for (i = 0; i < SIZE_SLOTS; i++) {
        sizes[i].size = 0xffffffffu;
    }
    inited = 1;
}

static inline uint32_t ptr_hash(void *p)
{
    uint32_t x = (uint32_t)(uintptr_t)p >> 3;
    x ^= x >> 15;
    x *= 0x2545f491u;
    x ^= x >> 13;
    return x & TBL_MASK;
}

/* ---- site table ------------------------------------------------------- */

static uint32_t site_lookup(const uint32_t *pc)
{
    uint32_t h = 0, i, j;
    for (i = 0; i < SITE_PCS; i++) {
        h = h * 0x01000193u ^ pc[i];
    }
    h &= (SITE_SLOTS - 1u);
    for (i = 0; i < SITE_SLOTS; i++) {
        uint32_t s = (h + i) & (SITE_SLOTS - 1u);
        if (!sites[s].used) {
            for (j = 0; j < SITE_PCS; j++) {
                sites[s].pc[j] = pc[j];
            }
            sites[s].used = 1;
            return s;
        }
        if (memcmp(sites[s].pc, pc, sizeof(uint32_t) * SITE_PCS) == 0) {
            return s;
        }
    }
    stat_site_overflow++;
    return SITE_SLOTS; /* sentinel: unattributed */
}

/* Conservative upward stack scan.  Called with interrupts still on -- it only
 * reads this thread's own live frames. */
static void capture_site(uint32_t *pc)
{
    uintptr_t lo = (uintptr_t)bsp_section_text_begin;
    uintptr_t hi = (uintptr_t)bsp_section_text_end;
    uintptr_t sp = (uintptr_t)__builtin_frame_address(0);
    unsigned found = 0, i;

    for (i = 0; i < SITE_PCS; i++) {
        pc[i] = 0;
    }
    sp = (sp + 3u) & ~(uintptr_t)3u;
    for (i = 0; i < SCAN_WORDS && found < SITE_PCS; i++) {
        uintptr_t w = *(volatile uintptr_t *)(sp + i * 4u);
        if (w > lo && w < hi) {
            pc[found++] = (uint32_t)w;
        }
    }
}

/* ---- size index ------------------------------------------------------- */

static void size_add(uint32_t sz, int delta)
{
    uint32_t h = (sz * 2654435761u) & SIZE_MASK;
    uint32_t i;
    for (i = 0; i < SIZE_SLOTS; i++) {
        uint32_t s = (h + i) & SIZE_MASK;
        if (sizes[s].size == sz) {
            if (delta > 0) {
                sizes[s].live++;
                sizes[s].bytes += sz;
            } else if (sizes[s].live) {
                sizes[s].live--;
                sizes[s].bytes -= sz;
            }
            return;
        }
        if (sizes[s].size == 0xffffffffu) {
            if (delta < 0) {
                return;
            }
            sizes[s].size = sz;
            sizes[s].live = 1;
            sizes[s].bytes = sz;
            return;
        }
    }
    stat_size_overflow++;
}

/* ---- record / forget -------------------------------------------------- */

static void record(void *p, size_t sz)
{
    uint32_t pc[SITE_PCS];
    uint32_t site, h, i, c;

    if (!p) {
        return;
    }
    capture_site(pc);

    c = irq_off();
    if (!inited) {
        heapattr_init();
    }
    site = site_lookup(pc);
    h = ptr_hash(p);
    for (i = 0; i < TBL_SIZE; i++) {
        uint32_t s = (h + i) & TBL_MASK;
        if (tbl[s].p == NULL || tbl[s].p == TOMBSTONE) {
            tbl[s].p = p;
            tbl[s].size = (uint32_t)sz;
            tbl[s].site = site;
            tbl_used++;
            stat_alloc++;
            live_blocks++;
            live_bytes += (uint32_t)sz;
            size_add((uint32_t)sz, +1);
            if (site < SITE_SLOTS) {
                sites[site].live++;
                sites[site].bytes += (uint32_t)sz;
                sites[site].total++;
            }
            irq_on(c);
            return;
        }
        if (tbl[s].p == p) {
            /* Should not happen: a live pointer handed out twice. */
            break;
        }
    }
    stat_tbl_overflow++;
    irq_on(c);
}

static void forget(void *p)
{
    uint32_t h, i, c;

    if (!p) {
        return;
    }
    c = irq_off();
    h = ptr_hash(p);
    for (i = 0; i < TBL_SIZE; i++) {
        uint32_t s = (h + i) & TBL_MASK;
        if (tbl[s].p == p) {
            uint32_t sz = tbl[s].size;
            uint32_t site = tbl[s].site;
            /* Tombstone, not NULL: deleting to NULL would cut a collision
             * chain and orphan every entry behind it, which this measurement
             * would then read as a growing leak. */
            tbl[s].p = TOMBSTONE;
            tbl_used--;
            stat_free++;
            if (live_blocks) {
                live_blocks--;
            }
            live_bytes -= sz;
            size_add(sz, -1);
            if (site < SITE_SLOTS && sites[site].live) {
                sites[site].live--;
                sites[site].bytes -= sz;
            }
            irq_on(c);
            return;
        }
        if (tbl[s].p == NULL) {
            break;
        }
    }
    stat_untracked_free++;
    irq_on(c);
}

/* ---- the wrappers ----------------------------------------------------- */

void *__wrap_malloc(size_t n)
{
    void *p = __real_malloc(n);
    record(p, n);
    return p;
}

/* NO __wrap_calloc / __wrap_realloc.  RTEMS implements both in terms of
 * malloc()/free() from separate translation units (cpukit/libcsupport/src/
 * calloc.c, realloc.c), so the inner calls are wrapped here already.  Wrapping
 * the outer entry point as well recorded every calloc twice under one pointer
 * -- measured: 55 duplicate inserts per 10 s report on the first smoke boot,
 * which is what `tbl_ovf` counts. */

void __wrap_free(void *p)
{
    forget(p);
    __real_free(p);
}

int __wrap_posix_memalign(void **out, size_t align, size_t n)
{
    int rc = __real_posix_memalign(out, align, n);
    if (rc == 0 && out) {
        record(*out, n);
    }
    return rc;
}

void *__wrap_aligned_alloc(size_t align, size_t n)
{
    void *p = __real_aligned_alloc(align, n);
    record(p, n);
    return p;
}

/* ---- the report ------------------------------------------------------- */

static sizerec_t snap_sizes[SIZE_SLOTS];
static siterec_t snap_sites[SITE_SLOTS];

void epics_heapattr_report(unsigned seq, unsigned attempts, int full)
{
    uint32_t c, i, n;
    uint32_t s_alloc, s_free, s_unt, s_tblov, s_sitov, s_szov, s_blocks,
        s_bytes, s_used;

    c = irq_off();
    memcpy(snap_sizes, sizes, sizeof(sizes));
    memcpy(snap_sites, sites, sizeof(sites));
    s_alloc = stat_alloc;
    s_free = stat_free;
    s_unt = stat_untracked_free;
    s_tblov = stat_tbl_overflow;
    s_sitov = stat_site_overflow;
    s_szov = stat_size_overflow;
    s_blocks = live_blocks;
    s_bytes = live_bytes;
    s_used = tbl_used;
    irq_on(c);

    printf("HEAPATTR seq=%u attempts=%u live_blocks=%u live_bytes=%u "
           "alloc=%u free=%u untracked_free=%u tbl_used=%u "
           "tbl_ovf=%u site_ovf=%u size_ovf=%u\n",
           seq, attempts, (unsigned)s_blocks, (unsigned)s_bytes,
           (unsigned)s_alloc, (unsigned)s_free, (unsigned)s_unt,
           (unsigned)s_used, (unsigned)s_tblov, (unsigned)s_sitov,
           (unsigned)s_szov);

    n = 0;
    printf("HEAPATTR seq=%u sizes", seq);
    for (i = 0; i < SIZE_SLOTS; i++) {
        if (snap_sizes[i].size != 0xffffffffu && snap_sizes[i].live) {
            printf(" %u:%u", (unsigned)snap_sizes[i].size,
                   (unsigned)snap_sizes[i].live);
            if (++n % 16 == 0) {
                printf("\nHEAPATTR seq=%u sizes", seq);
            }
        }
    }
    printf("\n");

    if (!full) {
        return;
    }
    for (i = 0; i < SITE_SLOTS; i++) {
        unsigned j;
        if (!snap_sites[i].used || snap_sites[i].bytes < SITE_REPORT_MIN) {
            continue;
        }
        printf("HEAPATTR seq=%u site=%u live=%u bytes=%u total=%u pc",
               seq, (unsigned)i, (unsigned)snap_sites[i].live,
               (unsigned)snap_sites[i].bytes, (unsigned)snap_sites[i].total);
        for (j = 0; j < SITE_PCS; j++) {
            printf(" 0x%08x", (unsigned)snap_sites[i].pc[j]);
        }
        printf("\n");
    }
}
