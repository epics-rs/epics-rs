/*
 * The boundary set for `epics_rtems_build_boot_argv`, run by
 * `scripts/csrc-check.sh` on every CI runner.
 *
 * Written in C rather than as a Rust `#[test]` on purpose: linking this
 * translation unit into the crate's own targets would put a build-script
 * artefact on the link line of every host binary that depends on
 * `epics-rtems-boot` — `epics-ca-rs` depends on it unconditionally — and that
 * package's contract is that it costs a host build nothing. A standalone driver
 * keeps the blast radius at zero and still runs the real code.
 *
 * One case per boundary, not one per story: what breaks a tokeniser is the
 * quote/separator edges and the capacity edge, not a plausible command line.
 */

#include <stdio.h>
#include <string.h>

#include "boot_args.h"

static int failures = 0;

static void check_argv(const char *what, const char *cmdline, int cap,
                       const char *const *want, int want_argc)
{
    char buf[1024];
    char *argv[EPICS_RTEMS_MAX_BOOT_ARGS + 2];
    int argc;
    int i;

    if (cap > (int)(sizeof(argv) / sizeof(argv[0]))) {
        printf("FAIL %s: cap %d exceeds the driver's argv\n", what, cap);
        ++failures;
        return;
    }
    if (strlen(cmdline) >= sizeof(buf)) {
        printf("FAIL %s: cmdline does not fit the driver's buffer\n", what);
        ++failures;
        return;
    }
    strcpy(buf, cmdline);

    argc = epics_rtems_build_boot_argv(buf, argv, cap);

    if (argc != want_argc) {
        printf("FAIL %s: argc %d, want %d\n", what, argc, want_argc);
        ++failures;
        return;
    }
    for (i = 0; i < want_argc; ++i) {
        if (argv[i] == NULL || strcmp(argv[i], want[i]) != 0) {
            printf("FAIL %s: argv[%d] = [%s], want [%s]\n", what, i,
                   argv[i] == NULL ? "(null)" : argv[i], want[i]);
            ++failures;
            return;
        }
    }
    if (argv[argc] != NULL) {
        printf("FAIL %s: argv[%d] is not NULL\n", what, argc);
        ++failures;
        return;
    }
    printf("ok   %s\n", what);
}

#define CAP ((int)(EPICS_RTEMS_MAX_BOOT_ARGS + 2))

int main(void)
{
    /* An unset EPICS_RTEMS_CMDLINE is the default, so this is every image
     * nobody configured: argv[0] alone, and the IOC takes its own defaults. */
    {
        static const char *const want[] = {"rtems-ioc"};
        check_argv("empty command line yields argv[0] alone", "", CAP, want, 1);
    }
    {
        static const char *const want[] = {"rtems-ioc", "EPICS_CA_SERVER_PORT=5099"};
        check_argv("one token", "EPICS_CA_SERVER_PORT=5099", CAP, want, 2);
    }
    /* Runs of separators, and both kinds, collapse — a hand-edited U-Boot
     * variable is full of them. */
    {
        static const char *const want[] = {"rtems-ioc", "A=1", "B=2", "/db/site.db"};
        check_argv("mixed and repeated separators collapse",
                   "  A=1\t\t B=2   /db/site.db  ", CAP, want, 4);
    }
    /* The case the quoting exists for: an address LIST is one value. Split
     * naively, "10.0.2.3" would arrive as a database file name. */
    {
        static const char *const want[] = {"rtems-ioc",
                                           "EPICS_CA_ADDR_LIST=10.0.2.2 10.0.2.3"};
        check_argv("a quoted value keeps its spaces",
                   "EPICS_CA_ADDR_LIST=\"10.0.2.2 10.0.2.3\"", CAP, want, 2);
    }
    {
        static const char *const want[] = {"rtems-ioc",
                                           "EPICS_CA_ADDR_LIST=10.0.2.2 10.0.2.3"};
        check_argv("quoting the whole assignment is the same token",
                   "\"EPICS_CA_ADDR_LIST=10.0.2.2 10.0.2.3\"", CAP, want, 2);
    }
    {
        static const char *const want[] = {"rtems-ioc", "X=a b"};
        check_argv("single quotes group too", "X='a b'", CAP, want, 2);
    }
    /* A quote group opens and closes mid-token; the quotes leave the token and
     * the halves join. */
    {
        static const char *const want[] = {"rtems-ioc", "ab cd", "e"};
        check_argv("a group closing mid-token rejoins", "a\"b c\"d e", CAP, want, 3);
    }
    /* The other quote character inside a group is data, not syntax. */
    {
        static const char *const want[] = {"rtems-ioc", "X=it's here"};
        check_argv("the other quote inside a group is data",
                   "\"X=it's here\"", CAP, want, 2);
    }
    /* An unterminated quote runs to the end rather than dropping the tail: a
     * truncated EPICS_CA_ADDR_LIST is a wrong address list. */
    {
        static const char *const want[] = {"rtems-ioc", "X=a b"};
        check_argv("an unterminated quote runs to the end", "X=\"a b", CAP, want, 2);
    }
    /* An empty quoted string is still a token — `X=` and `""` both reach the
     * IOC, which is what lets a site clear an inherited value. */
    {
        static const char *const want[] = {"rtems-ioc", "", "Y=2"};
        check_argv("an empty quoted token survives", "\"\" Y=2", CAP, want, 3);
    }
    /* The capacity edge. cap 3 holds argv[0], one token and the NULL, so the
     * second token is dropped and reported. */
    {
        static const char *const want[] = {"rtems-ioc", "A=1"};
        check_argv("tokens past capacity are dropped, not written",
                   "A=1 B=2 C=3", 3, want, 2);
    }
    /* Exactly full is not overflow. */
    {
        static const char *const want[] = {"rtems-ioc", "A=1", "B=2"};
        check_argv("a full argv is not an overflow", "A=1 B=2", 4, want, 3);
    }

    if (failures != 0) {
        printf("\n%d boot-argument case(s) failed\n", failures);
        return 1;
    }
    printf("\nall boot-argument cases passed\n");
    return 0;
}
