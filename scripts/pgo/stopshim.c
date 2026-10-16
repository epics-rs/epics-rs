#include <pthread.h>
#include <stdlib.h>
#include <unistd.h>
/* LD_PRELOAD for PGO training: exit(0) once the file $PGO_STOP exists. The
 * IOCs have no shutdown path and the profile runtime writes its .profraw only
 * from an atexit handler, so a signalled IOC leaves an empty profile. */
static void *watch(void *f) {
    while (access((const char *)f, F_OK)) usleep(100000);
    exit(0);
}
__attribute__((constructor)) static void start(void) {
    const char *f = getenv("PGO_STOP");
    pthread_t t;
    if (f && !pthread_create(&t, 0, watch, (void *)f)) pthread_detach(t);
}
