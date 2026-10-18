/* LD_PRELOADed into the oracle IOC: a thread's writes to stderr are held
 * until they end a line or are fflushed, then go out in one write(2).
 * StreamDebugClass::print (StreamError.cc:174-193) prints one debug line
 * in up to six calls and fflushes after the last, so on a plain stderr
 * another thread's line can land between the pieces; one write per line
 * cannot be split (writes to a regular file are atomic on Linux). */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static __thread char buf[8192];
static __thread size_t len;

static void emit(void)
{
    size_t off = 0;
    while (off < len) {
        ssize_t n = write(2, buf + off, len - off);
        if (n <= 0) break;
        off += n;
    }
    len = 0;
}

static void add(const char *s, size_t n)
{
    while (n) {
        size_t k = n < sizeof buf - len ? n : sizeof buf - len;
        memcpy(buf + len, s, k);
        len += k, s += k, n -= k;
        if (len == sizeof buf) emit();
    }
    if (len && buf[len - 1] == '\n') emit();
}

static int vadd(const char *fmt, va_list ap)
{
    char tmp[sizeof buf];
    int n = vsnprintf(tmp, sizeof tmp, fmt, ap);
    if (n > 0) add(tmp, (size_t)n < sizeof tmp ? (size_t)n : sizeof tmp - 1);
    return n;
}

#define REAL(name) ((__typeof__(&name))dlsym(RTLD_NEXT, #name))

int vfprintf(FILE *fp, const char *fmt, va_list ap)
{
    if (fp != stderr) return REAL(vfprintf)(fp, fmt, ap);
    return vadd(fmt, ap);
}

int __vfprintf_chk(FILE *fp, int flag, const char *fmt, va_list ap)
{
    (void)flag;
    return vfprintf(fp, fmt, ap);
}

int fprintf(FILE *fp, const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    int n = vfprintf(fp, fmt, ap);
    va_end(ap);
    return n;
}

int __fprintf_chk(FILE *fp, int flag, const char *fmt, ...)
{
    va_list ap;
    (void)flag;
    va_start(ap, fmt);
    int n = vfprintf(fp, fmt, ap);
    va_end(ap);
    return n;
}

int fputs(const char *s, FILE *fp)
{
    if (fp != stderr) return REAL(fputs)(s, fp);
    add(s, strlen(s));
    return 0;
}

int fputc(int c, FILE *fp)
{
    if (fp != stderr) return REAL(fputc)(c, fp);
    char ch = (char)c;
    add(&ch, 1);
    return (unsigned char)c;
}

int putc(int c, FILE *fp)
{
    return fputc(c, fp);
}

size_t fwrite(const void *p, size_t size, size_t n, FILE *fp)
{
    if (fp != stderr) return REAL(fwrite)(p, size, n, fp);
    add(p, size * n);
    return n;
}

int fflush(FILE *fp)
{
    if (fp == stderr || fp == NULL) emit();
    return REAL(fflush)(fp);
}

__attribute__((destructor)) static void at_exit(void)
{
    emit();
}
