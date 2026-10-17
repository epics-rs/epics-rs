/* Golden bytes for sscan R2-12 writeXDR.c: argv[1] = output file.
 * Built on x86_64 Linux against writeXDR.c and the epics-base headers;
 * the output is write_xdr.bin, which save_data::write_xdr tests against. */
#include <stdio.h>
#include "writeXDR.h"
int main(int argc, char **argv) {
    FILE *fd = fopen(argv[1], "wb+");
    char c1 = 'A', c2 = (char)0xFF;
    short s = -2;
    int i = 0x01020304;
    long l = 0x1122334455667788L;
    float f = 1.5f;
    double d = -2.25;
    char *empty = "", *abc = "abc", *four = "four", *five = "fives";
    float fv[3] = {0.5f, -1.0f, 3.25f};
    double dv[2] = {1e-300, 6.02e23};
    long pos;
    write_XDR_Init();
    writeXDR_char(fd, &c1); writeXDR_char(fd, &c2);
    writeXDR_short(fd, &s); writeXDR_int(fd, &i); writeXDR_long(fd, &l);
    writeXDR_float(fd, &f); writeXDR_double(fd, &d);
    writeXDR_counted_string(fd, &empty); writeXDR_counted_string(fd, &abc);
    writeXDR_counted_string(fd, &four); writeXDR_counted_string(fd, &five);
    writeXDR_string(fd, &five, 2);
    writeXDR_vector(fd, (char *)fv, 3, sizeof(float), (xdrproc_t)writeXDR_float);
    writeXDR_vector(fd, (char *)dv, 2, sizeof(double), (xdrproc_t)writeXDR_double);
    pos = writeXDR_getpos(fd);
    /* seek past the end and write: the gap reads back as zeros */
    writeXDR_setpos(fd, pos + 8);
    writeXDR_int(fd, &i);
    /* patch an earlier word */
    writeXDR_setpos(fd, 4);
    writeXDR_short(fd, &s);
    printf("%ld\n", pos);
    fclose(fd);
    return 0;
}
