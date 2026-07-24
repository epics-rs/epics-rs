/* rtems-cside panel, session 5: name the epicsEvent blocks that leak, by
 * address AND by creating call site, instead of inferring them from a count.
 *
 * APP CODE PLUS ONE LINK FLAG.  Nothing in EPICS base or RTEMS is patched.
 * The image is linked with
 *     -Wl,--wrap=epicsEventCreate -Wl,--wrap=epicsEventDestroy
 * so every reference to those two symbols - from libCom, dbCore, rsrv and ca
 * alike - resolves to the __wrap_ entry points below.  Each entry point calls
 * the real one and records (op, event address, caller PC).  The caller PC is
 * __builtin_return_address(0), which for epicsEventCreate is the true call
 * site: all five predicted rsrv-cycle sites call epicsEventCreate directly,
 * not through epicsEventMustCreate.
 *
 * Recording is OFF until `evtrace on` is run from iocsh, so nothing in the
 * boot path is touched and no lock is taken before the IOC is up.
 *
 * Declared in ~/rtems-cside/DEVIATIONS.md.
 */
#include <stdio.h>
#include <string.h>

#include <iocsh.h>
#include <epicsExport.h>
#include <epicsEvent.h>
#include <epicsThread.h>
#include <cadef.h>

#define EVT_MAX 20000

typedef struct evtRec {
    void *addr;
    void *caller;
    char  op;
} evtRec;

static evtRec evtBuf[EVT_MAX];
static int    evtOn;      /* 0 = record nothing */
static int    evtN;       /* next slot; may run past EVT_MAX (overflow count) */

epicsEventId __real_epicsEventCreate(epicsEventInitialState initialState);
void         __real_epicsEventDestroy(epicsEventId id);

static void evtRecord(char op, void *addr, void *caller)
{
    int i;
    if (!evtOn)
        return;
    i = __sync_fetch_and_add(&evtN, 1);
    if (i >= EVT_MAX)
        return;
    evtBuf[i].addr   = addr;
    evtBuf[i].caller = caller;
    evtBuf[i].op     = op;
}

epicsEventId __wrap_epicsEventCreate(epicsEventInitialState initialState)
{
    void *caller = __builtin_return_address(0);
    epicsEventId id = __real_epicsEventCreate(initialState);
    evtRecord('C', (void *)id, caller);
    return id;
}

void __wrap_epicsEventDestroy(epicsEventId id)
{
    evtRecord('D', (void *)id, __builtin_return_address(0));
    __real_epicsEventDestroy(id);
}

/* ------------------------------------------------------------------ */
/* evtrace on | off | reset | dump | count                            */

static const iocshArg evtraceArg0 = {"subcommand", iocshArgString};
static const iocshArg * const evtraceArgs[1] = {&evtraceArg0};
static const iocshFuncDef evtraceDef = {"evtrace", 1, evtraceArgs};

static void evtraceCall(const iocshArgBuf *args)
{
    const char *cmd = args[0].sval ? args[0].sval : "count";
    int n, i;

    if (!strcmp(cmd, "on")) {
        evtOn = 1;
        printf("EVTRACE on n=%d\n", evtN);
    } else if (!strcmp(cmd, "off")) {
        evtOn = 0;
        printf("EVTRACE off n=%d\n", evtN);
    } else if (!strcmp(cmd, "reset")) {
        evtOn = 0;
        evtN = 0;
        printf("EVTRACE reset\n");
    } else if (!strcmp(cmd, "count")) {
        printf("EVTRACE count n=%d cap=%d on=%d\n", evtN, EVT_MAX, evtOn);
    } else if (!strcmp(cmd, "dump")) {
        n = evtN;
        printf("EVTRACE dump begin n=%d cap=%d overflow=%d\n",
               n, EVT_MAX, n > EVT_MAX ? n - EVT_MAX : 0);
        if (n > EVT_MAX)
            n = EVT_MAX;
        for (i = 0; i < n; i++) {
            printf("EVT %d %c %p %p\n", i, evtBuf[i].op,
                   evtBuf[i].addr, evtBuf[i].caller);
            if ((i & 63) == 63)
                fflush(stdout);
        }
        printf("EVTRACE dump end\n");
    } else {
        printf("EVTRACE usage: evtrace on|off|reset|count|dump\n");
    }
    fflush(stdout);
}

/* ------------------------------------------------------------------ */
/* caloop N pv mode : drive libca client virtual circuits.
 *   mode 0 - ca_context_create + ca_context_destroy, no channel: the
 *            per-context cost with NO virtual circuit ever built.
 *   mode 1 - the same, plus one channel connected to `pv` (ca_pend_io),
 *            which builds exactly one virtual circuit per iteration.
 * The difference between the two is the per-circuit cost.               */

static const iocshArg caloopArg0 = {"n", iocshArgInt};
static const iocshArg caloopArg1 = {"pv", iocshArgString};
static const iocshArg caloopArg2 = {"mode", iocshArgInt};
static const iocshArg * const caloopArgs[3] = {&caloopArg0, &caloopArg1, &caloopArg2};
static const iocshFuncDef caloopDef = {"caloop", 3, caloopArgs};

static void caloopCall(const iocshArgBuf *args)
{
    int n = args[0].ival;
    const char *pv = args[1].sval;
    int mode = args[2].ival;
    int i, conn = 0, fail = 0, ctxfail = 0;

    for (i = 0; i < n; i++) {
        if (ca_context_create(ca_disable_preemptive_callback) != ECA_NORMAL) {
            ctxfail++;
            continue;
        }
        if (mode) {
            chid ch = NULL;
            if (ca_create_channel(pv, NULL, NULL, 0, &ch) == ECA_NORMAL) {
                if (ca_pend_io(5.0) == ECA_NORMAL &&
                    ca_state(ch) == cs_conn)
                    conn++;
                else
                    fail++;
            } else {
                fail++;
            }
        }
        ca_context_destroy();
    }
    printf("CALOOP done n=%d mode=%d connected=%d failed=%d ctxfail=%d\n",
           n, mode, conn, fail, ctxfail);
    fflush(stdout);
}

static void ciocEvTraceRegistrar(void)
{
    iocshRegister(&evtraceDef, evtraceCall);
    iocshRegister(&caloopDef, caloopCall);
}
epicsExportRegistrar(ciocEvTraceRegistrar);
