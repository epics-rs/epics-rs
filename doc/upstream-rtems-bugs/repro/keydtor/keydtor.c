/*
 * keydtor.c - does RTEMS drain destructor-LESS pthread key values before
 * running the other keys' destructors?
 *
 * POSIX (IEEE Std 1003.1, pthread_key_create) says a key created with a NULL
 * destructor is simply never called back; nothing licenses the implementation
 * to clear its value while another key's destructor is still running. So
 * pthread_getspecific(A) inside B's destructor must return the value the
 * thread stored, in every destructor round.
 *
 * One source, two images:
 *   * arm-rtems6 kernel image (POSIX_Init entry, config block at the bottom)
 *   * native Linux/glibc binary (main entry) as the control
 *
 * Both run the same two variants back to back, because the suspicion is that
 * the answer depends on the order the keys were created in (RTEMS walks the
 * thread's key/value pairs in a tree keyed by the key id):
 *   variant Afirst : A (no destructor) created first, then B (destructor)
 *   variant Bfirst : B (destructor) created first, then A (no destructor)
 *
 * B's destructor re-arms itself exactly once, which forces a second destructor
 * round; A is read in both rounds.
 */

#include <pthread.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <limits.h>

#ifndef KEYDTOR_PLATFORM
#define KEYDTOR_PLATFORM "unknown"
#endif

#define A_VAL ((void *)(uintptr_t)0xA5A5A5A5u)
#define B_VAL ((void *)(uintptr_t)0xB0B0B0B0u)
#define B_REARM ((void *)(uintptr_t)0xB1B1B1B1u)

static pthread_key_t kA; /* no destructor */
static pthread_key_t kB; /* destructor b_dtor */

static const char *variant;  /* "Afirst" / "Bfirst" */
static int dtor_round;       /* 1 on the first call, 2 after the re-arm */
static int rearmed;          /* re-arm exactly once */

static void say(const char *fmt, ...)
    __attribute__((format(printf, 1, 2)));

static void say(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    fputc('\n', stdout);
    fflush(stdout);
}

/*
 * B's destructor. Reads A through pthread_getspecific and reports it verbatim;
 * "A_live" is the whole measurement - true means the destructor-less key still
 * holds what the thread put there.
 */
static void b_dtor(void *val)
{
    void *a;

    dtor_round++;
    a = pthread_getspecific(kA);

    say("KEYDTOR-R%d-A=%p variant=%s B_arg=%p A_expected=%p A_live=%s",
        dtor_round, a, variant, val, A_VAL, (a == A_VAL) ? "yes" : "NO");

    if (!rearmed) {
        int rc;
        rearmed = 1;
        rc = pthread_setspecific(kB, B_REARM);
        say("KEYDTOR-R%d-REARM variant=%s rc=%d", dtor_round, variant, rc);
    }
}

static void *worker(void *arg)
{
    int rc_a, rc_b;

    (void)arg;
    rc_a = pthread_setspecific(kA, A_VAL);
    rc_b = pthread_setspecific(kB, B_VAL);
    say("KEYDTOR-THREAD variant=%s set_A_rc=%d set_B_rc=%d back_A=%p back_B=%p",
        variant, rc_a, rc_b, pthread_getspecific(kA), pthread_getspecific(kB));
    pthread_exit(NULL);
    return NULL;
}

static void run_variant(int b_first)
{
    pthread_t t;
    pthread_attr_t attr;
    int rc;

    variant = b_first ? "Bfirst" : "Afirst";
    dtor_round = 0;
    rearmed = 0;

    if (b_first) {
        rc = pthread_key_create(&kB, b_dtor);
        if (rc == 0) {
            rc = pthread_key_create(&kA, NULL);
        }
    } else {
        rc = pthread_key_create(&kA, NULL);
        if (rc == 0) {
            rc = pthread_key_create(&kB, b_dtor);
        }
    }
    if (rc != 0) {
        say("KEYDTOR-FAIL variant=%s pthread_key_create rc=%d", variant, rc);
        return;
    }

    say("KEYDTOR-VARIANT variant=%s created=%s kA=%lu kB=%lu dtor_iterations=%d",
        variant, b_first ? "B,A" : "A,B", (unsigned long)kA, (unsigned long)kB,
        (int)PTHREAD_DESTRUCTOR_ITERATIONS);

    pthread_attr_init(&attr);
    pthread_attr_setstacksize(&attr, 64 * 1024);
    rc = pthread_create(&t, &attr, worker, NULL);
    pthread_attr_destroy(&attr);
    if (rc != 0) {
        say("KEYDTOR-FAIL variant=%s pthread_create rc=%d", variant, rc);
        return;
    }
    pthread_join(t, NULL);

    say("KEYDTOR-VARIANT-END variant=%s rounds=%d expected_rounds=2",
        variant, dtor_round);

    pthread_key_delete(kA);
    pthread_key_delete(kB);
}

static int run_all(void)
{
    say("KEYDTOR-BEGIN platform=%s", KEYDTOR_PLATFORM);
    run_variant(0); /* A (no destructor) created first */
    run_variant(1); /* B (destructor) created first */
    say("KEYDTOR-DONE");
    return 0;
}

#ifdef __rtems__

#include <rtems.h>

void *POSIX_Init(void *argument)
{
    (void)argument;
    printf("\nkeydtor: POSIX_Init entered (RTEMS %s)\n",
           rtems_get_version_string());
    fflush(stdout);
    run_all();
    exit(0);
    return NULL;
}

/* --- RTEMS application configuration; <rtems/confdefs.h> must come last. --- */

#define CONFIGURE_APPLICATION_NEEDS_SIMPLE_CONSOLE_DRIVER
#define CONFIGURE_APPLICATION_NEEDS_CLOCK_DRIVER

#define CONFIGURE_MAXIMUM_POSIX_THREADS 8
#define CONFIGURE_MAXIMUM_POSIX_KEYS 16
#define CONFIGURE_MAXIMUM_POSIX_KEY_VALUE_PAIRS 64

#define CONFIGURE_MINIMUM_TASK_STACK_SIZE (16 * 1024)
#define CONFIGURE_EXTRA_TASK_STACKS (256 * 1024)

#define CONFIGURE_POSIX_INIT_THREAD_TABLE
#define CONFIGURE_POSIX_INIT_THREAD_ENTRY_POINT POSIX_Init
#define CONFIGURE_POSIX_INIT_THREAD_STACK_SIZE (64 * 1024)

#define CONFIGURE_INIT
#include <rtems/confdefs.h>

#else /* host control */

int main(void)
{
    return run_all();
}

#endif
