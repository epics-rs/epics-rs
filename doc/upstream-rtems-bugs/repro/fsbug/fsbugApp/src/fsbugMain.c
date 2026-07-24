/* Minimal reproduction of the EPICS base RTEMS boot crash.
 * Stock base, zero patches.  The only application content is the documented
 * "no filesystem is needed" declaration from base rtems_init.c line 216-217.
 * Expected: main() runs and prints.  Actual: FATAL exception before main().
 */
#include <stdio.h>
#include <epicsMemFs.h>

const epicsMemFS *epicsRtemsFSImage = NULL;   /* documented: none is needed */

int main(int argc, char **argv)
{
    (void)argc; (void)argv;
    printf("FSBUG: main() reached -- bug is NOT present\n");
    fflush(stdout);
    return 0;
}
