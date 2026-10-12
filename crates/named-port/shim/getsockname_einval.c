#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <sys/socket.h>
/* getsockname fails with EINVAL on a listening socket, as RTEMS's does on
 * the CA listener; every other socket is passed through. */
int getsockname(int fd, struct sockaddr *a, socklen_t *l) {
    static int (*real)(int, struct sockaddr *, socklen_t *);
    if (!real) real = dlsym(RTLD_NEXT, "getsockname");
    int acc = 0; socklen_t n = sizeof acc;
    if (getsockopt(fd, SOL_SOCKET, SO_ACCEPTCONN, &acc, &n) == 0 && acc) {
        errno = EINVAL;
        return -1;
    }
    return real(fd, a, l);
}
