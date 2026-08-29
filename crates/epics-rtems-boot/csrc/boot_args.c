/*
 * The boot command line's tokeniser. See boot_args.h for why it is its own
 * translation unit rather than a static function inside rtems_init.c.
 */

#include <stdio.h>

#include "boot_args.h"

int epics_rtems_build_boot_argv(char *cmdline, char **argv, int argv_cap)
{
    const int maxargc = argv_cap - 1;
    char *src = cmdline;
    char *dst = cmdline;
    int argc = 0;
    int refused = 0;

    argv[argc++] = "rtems-ioc";

    for (;;) {
        char quote = '\0';
        char *token;

        while (*src == ' ' || *src == '\t') {
            ++src;
        }
        if (*src == '\0') {
            break;
        }

        token = dst;
        while (*src != '\0' && (quote != '\0' || (*src != ' ' && *src != '\t'))) {
            if (quote == '\0' && (*src == '"' || *src == '\'')) {
                quote = *src++;
            } else if (quote != '\0' && *src == quote) {
                quote = '\0';
                ++src;
            } else {
                *dst++ = *src++;
            }
        }
        if (*src != '\0') {
            ++src; /* the separator, now free to hold this token's NUL */
        }
        *dst++ = '\0';

        if (argc < maxargc) {
            argv[argc++] = token;
        } else {
            ++refused;
        }
    }

    argv[argc] = NULL;

    if (refused != 0) {
        printf("rtems-boot: ***** boot command line has %d token(s) more than "
               "the %d this image can hold; THEY WERE DROPPED. Raise "
               "EPICS_RTEMS_MAX_BOOT_ARGS and rebuild. *****\n",
               refused, maxargc - 1);
    }
    return argc;
}
