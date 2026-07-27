/*
 * memquery.vxe -- what can a VxWorks 7 RTP actually ask the target about
 * memory, and does any answer track the reservation wall the CA worker pool
 * dies on?
 *
 * Two halves, one binary:
 *   1. query every candidate exactly once at startup (sysctl CTL_HW/CTL_KERN,
 *      memFindMax, memInfoGet, sysconf).
 *   2. walk a pthread ladder with argv[1]-sized stacks until pthread_create
 *      fails, re-reading every candidate as it climbs.
 *
 * The judgement is (2): the three stack classes wall at different thread
 * counts but at the same reserved bytes, so a metric that tracks the wall must
 * read the same at all three wall points.  A metric that only sees the RTP
 * heap will sit flat while the ladder climbs.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <pthread.h>
#include <sys/sysctl.h>
#include <sys/mman.h>
#include <time.h>
#include <memLib.h>
#include <memPartLib.h>

static void q (const char * label, int a, int b)
    {
    unsigned long long buf = 0;
    size_t len = sizeof (buf);
    int mib[2];
    int rc;

    mib[0] = a;
    mib[1] = b;
    errno = 0;
    rc = sysctl (mib, 2, &buf, &len, NULL, 0);
    if (rc != 0)
        printf ("MQ sysctl %-16s rc=%d errno=%d\n", label, rc, errno);
    else
        printf ("MQ sysctl %-16s len=%u val=%llu\n", label, (unsigned) len, buf);
    fflush (stdout);
    }

static void heap (const char * tag, unsigned long long reserved, int n)
    {
    MEM_PART_STATS s;
    size_t maxb;
    STATUS st;

    memset (&s, 0, sizeof (s));
    maxb = memFindMax ();
    st = memInfoGet (&s);
    printf ("MQ %-5s n=%-4d reserved=%-12llu memFindMax=%-12zu st=%d "
            "free=%-12zu maxfree=%-12zu alloc=%-12zu maxalloc=%zu\n",
            tag, n, reserved, maxb, (int) st,
            s.numBytesFree, s.maxBlockSizeFree, s.numBytesAlloc,
            s.maxBytesAlloc);
    fflush (stdout);
    }

static pthread_mutex_t gate = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t never = PTHREAD_COND_INITIALIZER;

static void * body (void * arg)
    {
    pthread_mutex_t own;

    /*
     * Mirror the Rust worker: a VxWorks pthread mutex materialises its
     * SEMAPHORE on FIRST LOCK, so a worker that never locks never charges the
     * object arena.  Lock one private mutex so this ladder consumes the
     * semaphore arena the way a leased worker does.
     */
    pthread_mutex_init (&own, NULL);
    if (pthread_mutex_lock (&own) != 0)
        {
        printf ("MQ SEMWALL errno=%d\n", errno);
        fflush (stdout);
        }

    pthread_mutex_lock (&gate);
    for (;;)
        pthread_cond_wait (&never, &gate);
    return NULL;
    }

static void all_queries (void)
    {
    q ("hw.physmem", CTL_HW, HW_PHYSMEM);
    q ("hw.usermem", CTL_HW, HW_USERMEM);
    q ("hw.pagesize", CTL_HW, HW_PAGESIZE);
    /*
     * sysctlCommon.h defines KERN_MEMTOP/KERN_PHYSMEMTOP only under
     * #ifndef _WRS_CONFIG_LP64, so a 64-bit RTP cannot name them.  Ask by
     * number anyway: whether the syscall still serves them is a measurement,
     * not something the header withdrawal settles.
     */
    q ("kern.memtop(41)", CTL_KERN, 41);
    q ("kern.physmemtop(42)", CTL_KERN, 42);
    }

/*
 * The one route left after sysctl/memLib/getrlimit: ask the address space by
 * taking from it.  Reserve PROT_NONE chunks until mmap fails, report the
 * total, give it all back.  If this ceiling predicts the pthread wall in the
 * same run, a startup probe can size the budget from the target; if it does
 * not, nothing an RTP can call knows where the wall is.
 */
#define MMAP_SLOTS 4096

static void * slots[MMAP_SLOTS];

static unsigned long long mmap_ceiling (size_t chunk)
    {
    unsigned long long total = 0;
    int held = 0;
    int i;

    while (held < MMAP_SLOTS)
        {
        void * p = mmap (NULL, chunk, PROT_NONE, MAP_PRIVATE | MAP_ANON,
                         MAP_ANON_FD, 0);
        if (p == MAP_FAILED)
            break;
        slots[held++] = p;
        total += (unsigned long long) chunk;
        }

    printf ("MQ mmap  chunk=%zu held=%d total=%llu errno=%d\n",
            chunk, held, total, errno);
    fflush (stdout);

    for (i = 0; i < held; i++)
        munmap (slots[i], chunk);

    return total;
    }

/*
 * The exhausting ladder above is not something a live IOC may run: while it
 * holds everything, any other thread's growth fails.  A single-mapping probe
 * holds only the candidate and gives it straight back.  Does the largest
 * single mapping reach the same ceiling the chunk ladder does, or does
 * fragmentation cap it lower?
 */
static void single_probe (void)
    {
    static const size_t sizes[] = {
        1024u << 20, 768u << 20, 512u << 20, 384u << 20, 320u << 20,
        288u << 20, 272u << 20, 264u << 20, 256u << 20, 192u << 20,
        128u << 20, 64u << 20
    };
    unsigned i;

    for (i = 0; i < sizeof (sizes) / sizeof (sizes[0]); i++)
        {
        struct timespec t0, t1;
        void * p;
        long us;

        clock_gettime (CLOCK_MONOTONIC, &t0);
        errno = 0;
        p = mmap (NULL, sizes[i], PROT_NONE, MAP_PRIVATE | MAP_ANON,
                  MAP_ANON_FD, 0);
        clock_gettime (CLOCK_MONOTONIC, &t1);
        us = (long) ((t1.tv_sec - t0.tv_sec) * 1000000
                     + (t1.tv_nsec - t0.tv_nsec) / 1000);

        if (p == MAP_FAILED)
            printf ("MQ single size=%-12zu FAIL errno=%d us=%ld\n",
                    sizes[i], errno, us);
        else
            {
            printf ("MQ single size=%-12zu OK   us=%ld\n", sizes[i], us);
            munmap (p, sizes[i]);
            }
        fflush (stdout);
        }
    }

int main (int argc, char ** argv)
    {
    size_t stack = (argc > 1) ? (size_t) strtoul (argv[1], NULL, 0) : (2u << 20);
    int step = (argc > 2) ? atoi (argv[2]) : 8;
    const char * mode = (argc > 3) ? argv[3] : "t";
    unsigned long long reserved = 0;
    int n = 0;

    printf ("MQ start stack=%zu step=%d mode=%s pagesize=%ld\n",
            stack, step, mode, sysconf (_SC_PAGESIZE));
    fflush (stdout);

    all_queries ();
    heap ("base", 0, 0);

    if (mode[0] == 'm')
        {
        /* Same chunk as one thread's stack, so the two ladders are comparable. */
        mmap_ceiling (stack);
        heap ("unmap", 0, 0);
        }
    else if (mode[0] == 'b')
        {
        single_probe ();
        heap ("unmap", 0, 0);
        }

    for (;;)
        {
        pthread_attr_t at;
        pthread_t t;
        int rc;

        pthread_attr_init (&at);
        pthread_attr_setstacksize (&at, stack);
        pthread_attr_setdetachstate (&at, PTHREAD_CREATE_DETACHED);
        errno = 0;
        rc = pthread_create (&t, &at, body, NULL);
        pthread_attr_destroy (&at);

        if (rc != 0)
            {
            printf ("MQ WALL n=%d rc=%d errno=%d reserved=%llu\n",
                    n, rc, errno, reserved);
            heap ("wall", reserved, n);
            all_queries ();
            break;
            }

        n++;
        reserved += (unsigned long long) stack;
        if ((n % step) == 0)
            heap ("step", reserved, n);
        }

    printf ("MQ done n=%d reserved=%llu\n", n, reserved);
    fflush (stdout);
    return 0;
    }
