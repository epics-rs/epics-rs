/* Transcript of sscan R2-12 req_file.c over a .req file: argv[1] = file,
 * argv[2] = macros. save_data::req_file's tests print the same walk and
 * compare with req_file_<name>.txt, this program's output on x86_64. */
#include <stdio.h>
#include "req_file.h"

static long stuck(REQ_FILE *rf, long before) {
    long now = ftell(rf->fd);
    if (now == before && !eos(rf)) { printf("  stuck at %d\n", current(rf)); req_readChar(rf); req_skipComment(rf); }
    return ftell(rf->fd);
}

int main(int argc, char **argv) {
    static char *sections[] = {"prefix", "status", "message", "filename", "fullPathName",
        "counter", "fileSystem", "subdir", "basename", "realTime1D", "noSuch",
        "averyveryveryverylon", "noclose", NULL};
    char b1[61], b2[61];
    char **s;
    const char *name;
    long pos;
    int n;
    REQ_FILE *rf = req_open_file(argv[1], argv[2]);
    req_infoMacro(rf);
    for (s = sections; *s; s++) {
        n = req_gotoSection(rf, *s);
        printf("goto %s = %d\n", *s, n);
        if (n == 0) {
            n = req_readMacId(rf, b1, 61);
            printf("  id %d '%s' eos=%d cur=%d\n", n, b1, eos(rf), current(rf));
        }
    }
    if (req_gotoSection(rf, "scanRecord") == 0) {
        pos = ftell(rf->fd);
        while (!eos(rf)) {
            n = req_readMacId(rf, b1, 61);
            printf("  scan %d '%s'\n", n, b1);
            if (current(rf) == ',') {
                req_readChar(rf);
                n = req_readMacId(rf, b2, 61);
                printf("  , %d '%s'\n", n, b2);
            }
            pos = stuck(rf, pos);
        }
    }
    if (req_gotoSection(rf, "extraPV") == 0) {
        pos = ftell(rf->fd);
        while (!eos(rf)) {
            n = req_readMacId(rf, b1, 61);
            printf("  extra %d '%s'\n", n, b1);
            if (current(rf) == '"') {
                n = req_readString(rf, b2, 61);
                printf("  desc %d '%s'\n", n, b2);
            }
            pos = stuck(rf, pos);
        }
    }
    req_gotoSection(rf, "prefix");
    while ((name = req_nextSection(rf))) {
        printf("next %s\n", name);
        pos = ftell(rf->fd);
        while (!eos(rf)) {
            n = req_readId(rf, b1, 40);
            printf("  %d '%s'\n", n, b1);
            pos = stuck(rf, pos);
        }
    }
    req_close_file(rf);
    return 0;
}
