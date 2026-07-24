/* rtems-cside panel: isolate the heap cost of ONE epicsEvent create/destroy
 * lifecycle on this target, so the per-CA-client-cycle leak can be divided by a
 * DIRECTLY measured per-block size instead of an assumed sizeof.
 *
 * APP CODE.  Nothing in EPICS base is patched by this file; it only calls base's
 * public epicsEvent API and registers an iocsh command, exactly as ciocSizes.c
 * does.  Declared in ~/rtems-cside/DEVIATIONS.md.
 */
#include <stdio.h>
#include <iocsh.h>
#include <epicsExport.h>
#include <epicsEvent.h>
#ifdef __rtems__
#include <rtems/thread.h>
#endif

/* evloop N : N x (epicsEventCreate + epicsEventDestroy), nothing else. */
static const iocshArg evloopArg0 = {"n", iocshArgInt};
static const iocshArg * const evloopArgs[1] = {&evloopArg0};
static const iocshFuncDef evloopDef = {"evloop", 1, evloopArgs};
static void evloopCall(const iocshArgBuf *args)
{
    int n = args[0].ival;
    int i;
    for (i = 0; i < n; i++) {
        epicsEventId id = epicsEventCreate(epicsEventEmpty);
        if (!id) {
            printf("EVLOOP create FAILED at i=%d\n", i);
            break;
        }
        epicsEventDestroy(id);
    }
    printf("EVLOOP done n=%d\n", n);
    fflush(stdout);
}

/* evsize : the struct size the leaked wrapper is made of, so heap-block
 * overhead can be separated from sizeof(epicsEventOSD). */
static const iocshFuncDef evsizeDef = {"evsize", 0, NULL};
static void evsizeCall(const iocshArgBuf *args)
{
    (void)args;
#ifdef __rtems__
    printf("EVSIZE sizeof(rtems_binary_semaphore)=%u\n",
           (unsigned)sizeof(rtems_binary_semaphore));
#else
    printf("EVSIZE not-rtems\n");
#endif
    fflush(stdout);
}

static void ciocEvLoopRegistrar(void)
{
    iocshRegister(&evloopDef, evloopCall);
    iocshRegister(&evsizeDef, evsizeCall);
}
epicsExportRegistrar(ciocEvLoopRegistrar);
