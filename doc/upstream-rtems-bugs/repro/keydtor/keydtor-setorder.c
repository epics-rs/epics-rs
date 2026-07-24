/*
 * keydtor-setorder.c - supplement to keydtor.c.
 *
 * keydtor.c varied the KEY CREATION order and found A (destructor-less) drained
 * in both rounds and both orders on RTEMS. The kernel source explains why the
 * creation order made no difference, and points at a different axis:
 *
 *   cpukit/posix/src/keycreate.c:113 _POSIX_Keys_Run_destructors
 *     :122  node = _RBTree_Root( &the_thread->Keys.Key_value_pairs );
 *     :133  _RBTree_Extract( ... )
 *     :139  _POSIX_Keys_Key_value_free( key_value_pair );   <-- value gone here
 *     :147  if ( destructor != NULL && value != NULL ) ( *destructor )( value );
 *
 * The pair is extracted and freed BEFORE the destructor is called, and the pair
 * picked each iteration is the RBTree ROOT - which for a two-node tree is the
 * first node inserted, i.e. the first pthread_setspecific the thread made.
 * keydtor.c always set A then B, so A was always the root and always drained
 * first. This program varies that axis too: 2 creation orders x 2 setspecific
 * orders = 4 variants.
 *
 * Same source builds an arm-rtems6 image and a native glibc control.
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

static const char *variant;
static int set_b_first;
static int dtor_round;
static int rearmed;

static void say(const char *fmt, ...) __attribute__((format(printf, 1, 2)));

static void say(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    fputc('\n', stdout);
    fflush(stdout);
}

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
    if (set_b_first) {
        rc_b = pthread_setspecific(kB, B_VAL);
        rc_a = pthread_setspecific(kA, A_VAL);
    } else {
        rc_a = pthread_setspecific(kA, A_VAL);
        rc_b = pthread_setspecific(kB, B_VAL);
    }
    say("KEYDTOR-THREAD variant=%s set_order=%s set_A_rc=%d set_B_rc=%d "
        "back_A=%p back_B=%p",
        variant, set_b_first ? "B,A" : "A,B", rc_a, rc_b,
        pthread_getspecific(kA), pthread_getspecific(kB));
    pthread_exit(NULL);
    return NULL;
}

static void run_variant(int b_first, int b_set_first)
{
    static char name[32];
    pthread_t t;
    pthread_attr_t attr;
    int rc;

    snprintf(name, sizeof(name), "%s-set%s", b_first ? "Bfirst" : "Afirst",
             b_set_first ? "BA" : "AB");
    variant = name;
    set_b_first = b_set_first;
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

    say("KEYDTOR-VARIANT variant=%s created=%s set_order=%s kA=%lu kB=%lu",
        variant, b_first ? "B,A" : "A,B", b_set_first ? "B,A" : "A,B",
        (unsigned long)kA, (unsigned long)kB);

    pthread_attr_init(&attr);
    pthread_attr_setstacksize(&attr, 64 * 1024);
    rc = pthread_create(&t, &attr, worker, NULL);
    pthread_attr_destroy(&attr);
    if (rc != 0) {
        say("KEYDTOR-FAIL variant=%s pthread_create rc=%d", variant, rc);
        return;
    }
    pthread_join(t, NULL);

    say("KEYDTOR-VARIANT-END variant=%s rounds=%d expected_rounds=2", variant,
        dtor_round);

    pthread_key_delete(kA);
    pthread_key_delete(kB);
}

static int run_all(void)
{
    say("KEYDTOR-BEGIN platform=%s probe=setorder", KEYDTOR_PLATFORM);
    run_variant(0, 0);
    run_variant(0, 1);
    run_variant(1, 0);
    run_variant(1, 1);
    say("KEYDTOR-DONE");
    return 0;
}

#ifdef __rtems__

#include <rtems.h>

void *POSIX_Init(void *argument)
{
    (void)argument;
    printf("\nkeydtor-setorder: POSIX_Init entered (RTEMS %s)\n",
           rtems_get_version_string());
    fflush(stdout);
    run_all();
    exit(0);
    return NULL;
}

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

#else

int main(void)
{
    return run_all();
}

#endif
