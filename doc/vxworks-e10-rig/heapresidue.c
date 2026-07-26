/* heapresidue.c — live-block heap accounting for the E10 dial-attempt residue
 * differential on x86_64-wrs-vxworks.
 *
 * Linked into the RTP with
 *
 *   -Wl,--wrap=malloc  -Wl,--wrap=free      -Wl,--wrap=calloc
 *   -Wl,--wrap=realloc -Wl,--wrap=memalign  -Wl,--wrap=posix_memalign
 *   -Wl,--wrap=aligned_alloc
 *
 * so every public allocator entry point in the image lands here first.
 *
 * WHAT THIS ADDS OVER THE PRIOR ROUND'S heapprobe2.c.  That file counted CALLS
 * and requested BYTES per call site and nothing else: `__wrap_free` only
 * incremented a counter.  A residue measurement is a LIVE-bytes differential —
 * cumulative allocation counts answer "churn", not "residue" — so this file
 * carries the RTEMS rig's pointer table: every live block is recorded with its
 * requested size and its call site, and `free` removes it.  Two incremental
 * indexes (per requested size, per site) mean a report never walks the big
 * table.
 *
 * FOUR THINGS THIS TARGET FORCES, each measured rather than carried over from
 * the RTEMS rig (`repro/dialresidue/heapattr.c`):
 *
 *  1. WRAP SET.  RTEMS deliberately does NOT wrap calloc/realloc because its
 *     libcsupport implements both over the public malloc()/free(), so an outer
 *     wrapper double-counts.  MEASURED on this target in the prior round:
 *     malloc_in_calloc=0 and malloc_in_realloc=0 across a whole run — VxWorks
 *     7's libc is mimalloc-based and the public entry points dispatch to mi_*
 *     internals, never to each other.  Here calloc/realloc MUST be wrapped or
 *     their blocks are untracked.  `memalign` is a seventh entry point the
 *     RTEMS set omits and that Rust's over-aligned allocations actually reach
 *     (libc's vxworks `posix_memalign` is implemented over it).
 *
 *  2. CALL SITE.  RTEMS could not use __builtin_return_address(0): rustc emits
 *     A32 with LLVM's frame layout while the shim is -mthumb with gcc's, so the
 *     link inserts veneers and the address names the veneer.  x86_64 has no
 *     veneers — MEASURED, the return address resolves directly to Rust frames —
 *     so the conservative stack scan, SCAN_WORDS, SITE_PCS and the
 *     bsp_section_text bounds are all deleted.
 *
 *  3. LOCKING.  RTEMS used interrupt-off; an RTP cannot (intLock is kernel-only).
 *     A pthread mutex would allocate from inside the allocator and a spinlock
 *     deadlocks a uniprocessor under SCHED_FIFO.  Every table here is lock-free:
 *     slots are claimed with __atomic CAS and counters accumulate with
 *     __atomic_fetch_add.
 *
 *  4. THE REPORT MUST BE RE-CALLABLE.  The RTEMS report is a top-N pass that
 *     CONSUMES entries, so it cannot be called twice — fatal for a differential
 *     that samples every 10 s.  This one is a non-destructive threshold print.
 *
 * THE NESTING FLAGS ARE KEYED BY TASK, AND MUST NOT BE `__thread`.
 * `in_calloc` / `in_realloc` answer "is THIS thread inside calloc/realloc right
 * now", which is the only question that makes a nested malloc a double-insert.
 * They were plain globals in the first revision, so they also answered "is SOME
 * thread inside", and a concurrent thread's malloc counted as nested: the
 * per-attempt PVA image, which creates ~300 threads against 11.4 M mallocs,
 * reported in_realloc=800 with no real nesting anywhere (the exact
 * `alloc - free - realloc == live_blocks` identity held, which it could not
 * have if 800 pointers had been inserted twice).
 *
 * The obvious repair, `static __thread int`, is MEASURED FATAL on this target.
 * An image carrying it dies before `main`:
 *
 *     -> rtpSp "/host.host/pva-nofix-10s.vxe"
 *     0xffff800010fc2400 (iPva-nofix-10s): RTP 0xffff800010fb8000 has been
 *     deleted due to signal 11.
 *
 * with `tlsbase = 0x0000000000000000`, faulting on `MOV %RAX, %FS:[0xfffffff0]`
 * — the first `__thread` read — under this ED&R traceback:
 *
 *     _start -> __init -> __wr_need_frame_add -> __unw_getcontext
 *            -> _Mtx_init -> mtx_init -> semMCreate -> __wrap_malloc
 *
 * The C runtime's own startup allocates before the RTP's TLS base register is
 * set, so a wrapper that touches TLS is unreachable on exactly the path every
 * image must survive.  So the per-task cell is keyed by `taskIdSelf()` instead,
 * in a 64-slot table claimed by CAS: the cell means "this identified task is
 * inside", not "somebody is", and the false positive is gone by construction
 * rather than by discounting the number.
 *
 * `taskIdSelf()` is safe in that pre-TLS window and VxWorks says so itself —
 * disassembled from this image, `_taskWindTcbCurrent` branches on a global
 * TLS-ready flag and takes the `_taskTcbCurrentGet` syscall (`syscall 0x27a`)
 * when it is clear, reaching `_tlsTcbCurrentGet`'s `mov %fs:0x38,%rax` only
 * once TLS exists.  It is called at all only when the `nest_occupied` gate is
 * nonzero, which for essentially the whole run it is not.
 *
 * DELETE WITH TOMBSTONES, NOT NULL — carried over from RTEMS unchanged, because
 * the failure it prevents is the open-addressing invariant, not an OS property:
 * writing 0 into a slot cuts the collision chain and orphans every entry behind
 * it, and an orphaned entry reads as a block that is never freed — a fabricated
 * leak that grows.
 */
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>

void *__real_malloc(size_t);
void __real_free(void *);
void *__real_calloc(size_t, size_t);
void *__real_realloc(void *, size_t);
int __real_posix_memalign(void **, size_t, size_t);
void *__real_aligned_alloc(size_t, size_t);
void *__real_memalign(size_t, size_t);

/* ------------------------------------------------------------------ tables */

#define BLK_SLOTS 262144u /* power of two; 262144 * 16 B = 4 MiB of BSS */
#define BLK_MASK (BLK_SLOTS - 1u)
#define BLK_TOMB ((uintptr_t)1) /* never a real heap pointer */

#define SITE_SLOTS 4096u
#define SITE_MASK (SITE_SLOTS - 1u)

#define SIZE_SLOTS 8192u
#define SIZE_MASK (SIZE_SLOTS - 1u)

struct blk {
    uintptr_t p; /* 0 empty, BLK_TOMB tombstone, else the live pointer */
    uint32_t size;
    uint32_t site; /* index into sites[]; SITE_SLOTS means "unattributed" */
};

struct site {
    uintptr_t pc;        /* return address of the allocating call */
    uint64_t calls;      /* allocations ever made from this site */
    uint64_t bytes;      /* bytes ever requested from this site */
    int64_t live_blocks; /* not yet freed */
    int64_t live_bytes;
};

struct sizeclass {
    uint32_t key; /* requested size + 1; 0 means empty */
    uint64_t allocs;
    int64_t live_blocks;
    int64_t live_bytes;
};

static struct blk blocks[BLK_SLOTS];
static struct site sites[SITE_SLOTS];
static struct sizeclass sizes[SIZE_SLOTS];

static int64_t live_bytes, live_blocks;
static uint64_t n_malloc, n_free, n_calloc, n_realloc, n_pmemalign,
    n_alignedalloc, n_memalign;
static uint64_t untracked_free, blk_overflow, site_overflow, size_overflow;
static uint64_t malloc_in_calloc, malloc_in_realloc;

/* Knuth multiplicative, shifted past the allocator's alignment bits. */
static uint32_t mix(uintptr_t v)
{
    return (uint32_t)(((uint64_t)v * 2654435761u) >> 4);
}

/* ------------------------------------------------------ nesting, by task */

/* Declared here rather than by including <taskLib.h>, which drags the kernel
 * task API into a file that has to stay freestanding.  `TASK_ID` is `c_int` on
 * this target — the disassembly returns it in `eax`. */
extern int taskIdSelf(void);

#define NEST_SLOTS 64u

struct nest {
    uintptr_t key; /* (uint32_t)task id + 1; 0 means the slot is free */
    int in_calloc;
    int in_realloc;
};

static struct nest nests[NEST_SLOTS];
static int64_t nest_occupied;  /* slots currently claimed */
static uint64_t nest_overflow; /* enters that found no free slot */

/* +1 so that no task id maps to the free marker: the only 32-bit value that
 * could is 0xffffffff (`taskIdSelf` failing), and widened to 64 bits that is
 * 0x100000000, not 0. */
static uintptr_t nest_key(void)
{
    return (uintptr_t)(uint32_t)taskIdSelf() + 1u;
}

/* This task's slot, or NEST_SLOTS when it has none and (`claim` = 0) we are not
 * asking for one, or the table is full.  Two passes on purpose: the lookup pass
 * takes no CAS, so the malloc hook — which never claims — only reads. */
static uint32_t nest_slot(uintptr_t self, int claim)
{
    uint32_t i;
    for (i = 0; i < NEST_SLOTS; i++) {
        if (__atomic_load_n(&nests[i].key, __ATOMIC_ACQUIRE) == self) return i;
    }
    if (!claim) return NEST_SLOTS;
    for (i = 0; i < NEST_SLOTS; i++) {
        uintptr_t zero = 0;
        if (__atomic_load_n(&nests[i].key, __ATOMIC_RELAXED) != 0) continue;
        if (__atomic_compare_exchange_n(&nests[i].key, &zero, self, 0,
                                        __ATOMIC_ACQ_REL, __ATOMIC_RELAXED)) {
            __atomic_fetch_add(&nest_occupied, 1, __ATOMIC_RELAXED);
            return i;
        }
    }
    __atomic_fetch_add(&nest_overflow, 1, __ATOMIC_RELAXED);
    return NEST_SLOTS;
}

/* Returns the slot the matching `nest_leave` must be given. */
static uint32_t nest_enter(int is_realloc)
{
    uint32_t i = nest_slot(nest_key(), 1);
    if (i == NEST_SLOTS) return i;
    if (is_realloc)
        __atomic_fetch_add(&nests[i].in_realloc, 1, __ATOMIC_RELAXED);
    else
        __atomic_fetch_add(&nests[i].in_calloc, 1, __ATOMIC_RELAXED);
    return i;
}

static void nest_leave(uint32_t i, int is_realloc)
{
    if (i >= NEST_SLOTS) return;
    if (is_realloc)
        __atomic_fetch_sub(&nests[i].in_realloc, 1, __ATOMIC_RELAXED);
    else
        __atomic_fetch_sub(&nests[i].in_calloc, 1, __ATOMIC_RELAXED);
    /* Released only once this task is out of BOTH wrappers, so the slot cannot
     * pass to another task while a depth of ours still stands. */
    if (__atomic_load_n(&nests[i].in_calloc, __ATOMIC_RELAXED) == 0 &&
        __atomic_load_n(&nests[i].in_realloc, __ATOMIC_RELAXED) == 0) {
        __atomic_store_n(&nests[i].key, (uintptr_t)0, __ATOMIC_RELEASE);
        __atomic_fetch_sub(&nest_occupied, 1, __ATOMIC_RELAXED);
    }
}

/* The malloc hook's half: count this call as nested only if THIS task is the
 * one inside.  Gated on `nest_occupied`, which is 0 for essentially the whole
 * run, so the common path is one relaxed load and no `taskIdSelf` — and that
 * gate is also why the pre-TLS startup path never reaches a syscall. */
static void nest_note_malloc(void)
{
    uint32_t i;
    if (__atomic_load_n(&nest_occupied, __ATOMIC_RELAXED) == 0) return;
    i = nest_slot(nest_key(), 0);
    if (i == NEST_SLOTS) return;
    if (__atomic_load_n(&nests[i].in_calloc, __ATOMIC_RELAXED))
        __atomic_fetch_add(&malloc_in_calloc, 1, __ATOMIC_RELAXED);
    if (__atomic_load_n(&nests[i].in_realloc, __ATOMIC_RELAXED))
        __atomic_fetch_add(&malloc_in_realloc, 1, __ATOMIC_RELAXED);
}

/* ------------------------------------------------------------ site index */

/* Returns the slot index, or SITE_SLOTS when the table is full. */
static uint32_t site_intern(uintptr_t pc, size_t n)
{
    uint32_t h = mix(pc) & SITE_MASK;
    uint32_t i;
    if (pc == 0) return SITE_SLOTS;
    for (i = 0; i < SITE_SLOTS; i++) {
        uint32_t s = (h + i) & SITE_MASK;
        uintptr_t cur = __atomic_load_n(&sites[s].pc, __ATOMIC_ACQUIRE);
        if (cur == 0) {
            uintptr_t zero = 0;
            if (!__atomic_compare_exchange_n(&sites[s].pc, &zero, pc, 0,
                                             __ATOMIC_ACQ_REL,
                                             __ATOMIC_ACQUIRE)) {
                cur = __atomic_load_n(&sites[s].pc, __ATOMIC_ACQUIRE);
                if (cur != pc) continue;
            }
            cur = pc;
        }
        if (cur == pc) {
            __atomic_fetch_add(&sites[s].calls, 1, __ATOMIC_RELAXED);
            __atomic_fetch_add(&sites[s].bytes, (uint64_t)n, __ATOMIC_RELAXED);
            __atomic_fetch_add(&sites[s].live_blocks, 1, __ATOMIC_RELAXED);
            __atomic_fetch_add(&sites[s].live_bytes, (int64_t)n,
                               __ATOMIC_RELAXED);
            return s;
        }
    }
    __atomic_fetch_add(&site_overflow, 1, __ATOMIC_RELAXED);
    return SITE_SLOTS;
}

static void site_release(uint32_t s, size_t n)
{
    if (s >= SITE_SLOTS) return;
    __atomic_fetch_sub(&sites[s].live_blocks, 1, __ATOMIC_RELAXED);
    __atomic_fetch_sub(&sites[s].live_bytes, (int64_t)n, __ATOMIC_RELAXED);
}

/* ------------------------------------------------------------ size index */

static void size_take(size_t n)
{
    uint32_t key = (uint32_t)n + 1u;
    uint32_t h = mix((uintptr_t)key) & SIZE_MASK;
    uint32_t i;
    for (i = 0; i < SIZE_SLOTS; i++) {
        uint32_t s = (h + i) & SIZE_MASK;
        uint32_t cur = __atomic_load_n(&sizes[s].key, __ATOMIC_ACQUIRE);
        if (cur == 0) {
            uint32_t zero = 0;
            if (!__atomic_compare_exchange_n(&sizes[s].key, &zero, key, 0,
                                             __ATOMIC_ACQ_REL,
                                             __ATOMIC_ACQUIRE)) {
                cur = __atomic_load_n(&sizes[s].key, __ATOMIC_ACQUIRE);
                if (cur != key) continue;
            }
            cur = key;
        }
        if (cur == key) {
            __atomic_fetch_add(&sizes[s].allocs, 1, __ATOMIC_RELAXED);
            __atomic_fetch_add(&sizes[s].live_blocks, 1, __ATOMIC_RELAXED);
            __atomic_fetch_add(&sizes[s].live_bytes, (int64_t)n,
                               __ATOMIC_RELAXED);
            return;
        }
    }
    __atomic_fetch_add(&size_overflow, 1, __ATOMIC_RELAXED);
}

static void size_give(size_t n)
{
    uint32_t key = (uint32_t)n + 1u;
    uint32_t h = mix((uintptr_t)key) & SIZE_MASK;
    uint32_t i;
    for (i = 0; i < SIZE_SLOTS; i++) {
        uint32_t s = (h + i) & SIZE_MASK;
        uint32_t cur = __atomic_load_n(&sizes[s].key, __ATOMIC_ACQUIRE);
        if (cur == 0) return; /* never interned: it overflowed on the way in */
        if (cur == key) {
            __atomic_fetch_sub(&sizes[s].live_blocks, 1, __ATOMIC_RELAXED);
            __atomic_fetch_sub(&sizes[s].live_bytes, (int64_t)n,
                               __ATOMIC_RELAXED);
            return;
        }
    }
}

/* ----------------------------------------------------------- block table */

/* `p` is not yet visible to any other thread — it was returned by the real
 * allocator to THIS caller and has not been published — so nothing can free it
 * before the insert completes.  Concurrent INSERTS of different pointers are
 * the only race, and the CAS resolves those. */
static void blk_insert(void *vp, size_t n, uintptr_t pc)
{
    uintptr_t p = (uintptr_t)vp;
    uint32_t h, i, site;
    if (p == 0) return;
    site = site_intern(pc, n);
    size_take(n);
    __atomic_fetch_add(&live_blocks, 1, __ATOMIC_RELAXED);
    __atomic_fetch_add(&live_bytes, (int64_t)n, __ATOMIC_RELAXED);
    h = mix(p) & BLK_MASK;
    for (i = 0; i < BLK_SLOTS; i++) {
        uint32_t s = (h + i) & BLK_MASK;
        uintptr_t cur = __atomic_load_n(&blocks[s].p, __ATOMIC_ACQUIRE);
        if (cur == 0 || cur == BLK_TOMB) {
            if (__atomic_compare_exchange_n(&blocks[s].p, &cur, p, 0,
                                            __ATOMIC_ACQ_REL,
                                            __ATOMIC_ACQUIRE)) {
                blocks[s].size = (uint32_t)n;
                blocks[s].site = site;
                return;
            }
        }
    }
    /* Table full: undo, so live_bytes never counts a block the table cannot
     * later find and release.  An overflowed run is reported, not silently
     * skewed. */
    __atomic_fetch_add(&blk_overflow, 1, __ATOMIC_RELAXED);
    __atomic_fetch_sub(&live_blocks, 1, __ATOMIC_RELAXED);
    __atomic_fetch_sub(&live_bytes, (int64_t)n, __ATOMIC_RELAXED);
    size_give(n);
    site_release(site, n);
}

/* Returns 1 when the pointer was live and has been released. */
static int blk_remove(void *vp)
{
    uintptr_t p = (uintptr_t)vp;
    uint32_t h, i;
    if (p == 0) return 1; /* free(NULL) is a no-op, not an untracked free */
    h = mix(p) & BLK_MASK;
    for (i = 0; i < BLK_SLOTS; i++) {
        uint32_t s = (h + i) & BLK_MASK;
        uintptr_t cur = __atomic_load_n(&blocks[s].p, __ATOMIC_ACQUIRE);
        if (cur == 0) break; /* empty slot ends the chain; tombstones do not */
        if (cur == p) {
            size_t n = blocks[s].size;
            uint32_t site = blocks[s].site;
            if (!__atomic_compare_exchange_n(&blocks[s].p, &cur, BLK_TOMB, 0,
                                             __ATOMIC_ACQ_REL,
                                             __ATOMIC_ACQUIRE)) {
                continue;
            }
            __atomic_fetch_sub(&live_blocks, 1, __ATOMIC_RELAXED);
            __atomic_fetch_sub(&live_bytes, (int64_t)n, __ATOMIC_RELAXED);
            size_give(n);
            site_release(site, n);
            return 1;
        }
    }
    __atomic_fetch_add(&untracked_free, 1, __ATOMIC_RELAXED);
    return 0;
}

#define SITE() ((uintptr_t)__builtin_return_address(0))

/* -------------------------------------------------------------- wrappers */

void *__wrap_malloc(size_t n)
{
    void *p;
    __atomic_fetch_add(&n_malloc, 1, __ATOMIC_RELAXED);
    nest_note_malloc();
    p = __real_malloc(n);
    blk_insert(p, n, SITE());
    return p;
}

void __wrap_free(void *p)
{
    __atomic_fetch_add(&n_free, 1, __ATOMIC_RELAXED);
    blk_remove(p);
    __real_free(p);
}

void *__wrap_calloc(size_t a, size_t b)
{
    void *p;
    uint32_t slot;
    __atomic_fetch_add(&n_calloc, 1, __ATOMIC_RELAXED);
    slot = nest_enter(0);
    p = __real_calloc(a, b);
    nest_leave(slot, 0);
    blk_insert(p, a * b, SITE());
    return p;
}

void *__wrap_realloc(void *q, size_t n)
{
    void *p;
    uint32_t slot;
    __atomic_fetch_add(&n_realloc, 1, __ATOMIC_RELAXED);
    if (q) blk_remove(q);
    slot = nest_enter(1);
    p = __real_realloc(q, n);
    nest_leave(slot, 1);
    if (p) {
        blk_insert(p, n, SITE());
    } else if (q && n != 0) {
        /* realloc failed and `q` is still live: put it back, or the accounting
         * loses a block that was never freed. */
        blk_insert(q, n, SITE());
    }
    return p;
}

int __wrap_posix_memalign(void **out, size_t align, size_t n)
{
    int rc;
    __atomic_fetch_add(&n_pmemalign, 1, __ATOMIC_RELAXED);
    rc = __real_posix_memalign(out, align, n);
    if (rc == 0 && out) blk_insert(*out, n, SITE());
    return rc;
}

void *__wrap_aligned_alloc(size_t align, size_t n)
{
    void *p;
    __atomic_fetch_add(&n_alignedalloc, 1, __ATOMIC_RELAXED);
    p = __real_aligned_alloc(align, n);
    blk_insert(p, n, SITE());
    return p;
}

void *__wrap_memalign(size_t align, size_t n)
{
    void *p;
    __atomic_fetch_add(&n_memalign, 1, __ATOMIC_RELAXED);
    p = __real_memalign(align, n);
    blk_insert(p, n, SITE());
    return p;
}

/* ---------------------------------------------------------------- report */

/* Non-destructive, so a differential can call it every 10 s for an hour.
 *
 * `seq` is passed as an integer rather than as a formatted tag string on
 * purpose: a Rust-side `CString` would allocate from inside the very heap this
 * is reporting, and would be live at the instant the totals are printed.
 *
 * `detail` selects the two per-class tables.  The summary lines are cheap and
 * go out every call; the size and site tables run to a few hundred lines each,
 * so the caller asks for them at the census cadence (every 6th pass), which is
 * enough for a two-point differential and keeps the console log an order of
 * magnitude smaller. */
void heapresidue_report(unsigned seq, int detail)
{
    unsigned i, shown;
    printf("HEAPLIVE seq=%u live_bytes=%lld live_blocks=%lld alloc=%llu "
           "free=%llu untracked_free=%llu blk_ovf=%llu site_ovf=%llu "
           "size_ovf=%llu\n",
           seq, (long long)__atomic_load_n(&live_bytes, __ATOMIC_RELAXED),
           (long long)__atomic_load_n(&live_blocks, __ATOMIC_RELAXED),
           (unsigned long long)(n_malloc + n_calloc + n_realloc + n_pmemalign +
                                n_alignedalloc + n_memalign),
           (unsigned long long)n_free, (unsigned long long)untracked_free,
           (unsigned long long)blk_overflow, (unsigned long long)site_overflow,
           (unsigned long long)size_overflow);
    printf("HEAPCALL seq=%u malloc=%llu free=%llu calloc=%llu realloc=%llu "
           "pmemalign=%llu alignedalloc=%llu memalign=%llu in_calloc=%llu "
           "in_realloc=%llu nest_ovf=%llu\n",
           seq, (unsigned long long)n_malloc, (unsigned long long)n_free,
           (unsigned long long)n_calloc, (unsigned long long)n_realloc,
           (unsigned long long)n_pmemalign,
           (unsigned long long)n_alignedalloc,
           (unsigned long long)n_memalign,
           (unsigned long long)malloc_in_calloc,
           (unsigned long long)malloc_in_realloc,
           (unsigned long long)nest_overflow);
    if (!detail) return;
    shown = 0;
    for (i = 0; i < SIZE_SLOTS; i++) {
        uint32_t key = sizes[i].key;
        int64_t live = sizes[i].live_blocks;
        if (key && live != 0) {
            printf("HEAPSIZE seq=%u size=%u live=%lld bytes=%lld allocs=%llu\n",
                   seq, key - 1u, (long long)live,
                   (long long)sizes[i].live_bytes,
                   (unsigned long long)sizes[i].allocs);
            shown++;
        }
    }
    printf("HEAPSIZE seq=%u classes_printed=%u\n", seq, shown);
    shown = 0;
    for (i = 0; i < SITE_SLOTS; i++) {
        if (sites[i].pc && sites[i].live_bytes != 0) {
            printf("HEAPSITE seq=%u pc=0x%llx calls=%llu bytes=%llu live=%lld "
                   "livebytes=%lld\n",
                   seq, (unsigned long long)sites[i].pc,
                   (unsigned long long)sites[i].calls,
                   (unsigned long long)sites[i].bytes,
                   (long long)sites[i].live_blocks,
                   (long long)sites[i].live_bytes);
            shown++;
        }
    }
    printf("HEAPSITE seq=%u sites_printed=%u\n", seq, shown);
}
