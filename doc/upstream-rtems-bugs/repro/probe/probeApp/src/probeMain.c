/* Identify the descriptor libevent's poll backend spins on, and reproduce the
 * spin without libevent at all.
 */
#include <stdio.h>
#include <time.h>
#include <string.h>
#include <errno.h>
#include <limits.h>
#include <fcntl.h>
#include <pthread.h>
#include <unistd.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <netinet/in.h>
#ifdef __rtems__
#include <rtems/cpuuse.h>
#else
#define rtems_cpu_usage_reset()  do{}while(0)
#define rtems_cpu_usage_report() do{}while(0)
#endif
#include <event2/event.h>
#include <event2/thread.h>
#include <epicsMemFs.h>

const epicsMemFS *epicsRtemsFSImage = NULL;
int epicsRtemsMountLocalFilesystem(char **argv) { argv[1] = "/"; return 0; }

static double now_mono(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec * 1e-9;
}

/* ---- poll() interposition: snapshot the first and last call ---- */
extern int __real_poll(struct pollfd *, nfds_t, int);
static volatile long pc_calls;
static volatile int  pc_fd = -1, pc_rv, pc_rev, pc_err, pc_msec;
static void pc_reset(void){ pc_calls=0; pc_fd=-1; }
int __wrap_poll(struct pollfd *f, nfds_t n, int t)
{
    int rv = __real_poll(f, n, t);
    pc_calls++;
    if (n > 0) { pc_fd = f[0].fd; pc_rev = f[0].revents; }
    pc_rv = rv; pc_err = errno; pc_msec = t;
    return rv;
}

static void describe_fd(const char *what, int fd)
{
    int type = -1; socklen_t l = sizeof type;
    int issock = getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &l);
    struct stat st; int hasstat = fstat(fd, &st);
    printf("PROBE   fd %d: getsockopt(SO_TYPE)=%d (errno=%d, type=%d)  fstat=%d mode=0%o  [%s]\n",
           fd, issock, issock ? errno : 0, type, hasstat,
           hasstat == 0 ? (unsigned)st.st_mode : 0u, what);
    fflush(stdout);
}

static void timed_poll(const char *what, int fd, int msec)
{
    struct pollfd p; p.fd = fd; p.events = POLLIN; p.revents = 0;
    double t0 = now_mono();
    int rv = __real_poll(&p, 1, msec);
    int e = errno;
    double el = now_mono() - t0;
    printf("PROBE %-22s poll(fd=%d, POLLIN, %d ms) rv=%d errno=%d revents=0x%x elapsed=%.4f s -> %s\n",
           what, fd, msec, rv, e, p.revents, el,
           el > (msec/1000.0)*0.8 ? "BLOCKS correctly" : "RETURNS IMMEDIATELY (spin source)");
    fflush(stdout);
}

static void ka_cb(evutil_socket_t f, short w, void *a) { (void)f;(void)w;(void)a; }
static void *ev_worker(void *arg)
{
    (void)arg;
    struct event_config *conf = event_config_new();
    event_config_avoid_method(conf, "kqueue");
    struct event_base *b = event_base_new_with_config(conf);
    printf("PROBE ev base method=%s\n", event_base_get_method(b)); fflush(stdout);
    struct event *ka = event_new(b,-1,EV_TIMEOUT|EV_PERSIST,ka_cb,NULL);
    struct timeval tick = {1000,0}; event_add(ka,&tick);
    struct timeval quit = {2,0}; event_base_loopexit(b,&quit);
    event_base_loop(b, 0);
    event_free(ka); event_base_free(b);
    return NULL;
}

int main(int argc, char **argv)
{
    (void)argc; (void)argv;
    int pfd[2], sv[2], uf;
    struct sockaddr_in sa;

    printf("\nPROBE === A. what can this BSP poll on? ===\n"); fflush(stdout);

    uf = socket(AF_INET, SOCK_DGRAM, 0);
    memset(&sa,0,sizeof sa); sa.sin_family=AF_INET; sa.sin_port=htons(15999);
    sa.sin_addr.s_addr=htonl(INADDR_ANY);
    bind(uf,(struct sockaddr*)&sa,sizeof sa);
    describe_fd("udp socket", uf);
    timed_poll("udp socket", uf, 1000);

    if (pipe(pfd) == 0) {
        printf("PROBE pipe() -> read fd %d, write fd %d\n", pfd[0], pfd[1]);
        describe_fd("pipe read end", pfd[0]);
        timed_poll("pipe read end", pfd[0], 1000);
    } else {
        printf("PROBE pipe() FAILED errno=%d\n", errno);
    }
    fflush(stdout);

    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0) {
        printf("PROBE socketpair(AF_UNIX) -> %d, %d\n", sv[0], sv[1]);
        describe_fd("unix socketpair", sv[0]);
        timed_poll("unix socketpair", sv[0], 1000);
    } else {
        printf("PROBE socketpair(AF_UNIX) FAILED errno=%d\n", errno);
    }
    fflush(stdout);

    printf("\nPROBE === B. which fd does libevent's poll backend spin on? ===\n"); fflush(stdout);
    evthread_use_pthreads();
    pthread_t th;
    pc_reset();
    rtems_cpu_usage_reset();
    pthread_create(&th, NULL, ev_worker, NULL);
    double t0 = now_mono();
    for (int i = 0; i < 10; i++) { struct timespec r = {0,100000000}; nanosleep(&r, NULL); }
    double el = now_mono() - t0;
    pthread_join(th, NULL);
    printf("PROBE MAIN 10x100ms = %.3f s -> %s\n", el, el < 1.5 ? "normal" : "STARVED");
    printf("PROBE poll() calls=%ld  last: fd=%d msec=%d rv=%d revents=0x%x errno=%d\n",
           pc_calls, pc_fd, pc_msec, pc_rv, pc_rev, pc_err);
    fflush(stdout);
    if (pc_fd >= 0) { describe_fd("the fd libevent polls", pc_fd); timed_poll("libevent notify fd", pc_fd, 1000); }

    printf("\nPROBE DONE\n"); fflush(stdout);
    return 0;
}
