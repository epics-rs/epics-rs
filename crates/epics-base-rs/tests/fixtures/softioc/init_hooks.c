/* init_hooks.out is softIoc's run of init_hooks.cmd with this preloaded:
 *   cc -shared -fPIC -o hookprobe.so init_hooks.c -I$EPICS_BASE/include \
 *      -I$EPICS_BASE/include/os/Linux -I$EPICS_BASE/include/compiler/gcc \
 *      -L$EPICS_BASE/lib/linux-x86_64 -ldbCore -lCom
 *   EPICS_IOC_IGNORE_SERVERS=rsrv LD_PRELOAD=./hookprobe.so softIoc init_hooks.cmd \
 *      2>&1 </dev/null | grep -o 'HOOK .*'
 * Each initHook state with the IOC state and whether dbCaLinkInit has
 * started its dbCaLink thread; close_links.out is the same over
 * close_links.cmd, from `cd` here, with each close_links.db record's PACT
 * at the first two iocShutdown states. */
#include <stdio.h>
#include "initHooks.h"
#include "iocInit.h"
#include "epicsThread.h"
#include "dbAccess.h"
#include "dbCommon.h"

static const char *names[] = {"Void", "Building", "Built", "Running", "Paused"};

static const char *records[] = {"CL:ca", "CL:json", "CL:db", "CL:soft", "CL:plain"};

static void hook(initHookState state)
{
    unsigned i;
    fprintf(stderr, "HOOK %s %s calink=%d\n", initHookName(state),
        names[getIocState()], epicsThreadGetId("dbCaLink") != 0);
    if (state != initHookAtShutdown && state != initHookAfterCloseLinks)
        return;
    for (i = 0; i < sizeof records / sizeof *records; i++) {
        DBADDR addr;
        if (dbNameToAddr(records[i], &addr) == 0)
            fprintf(stderr, "HOOK PACT %s %s=%d\n", initHookName(state),
                records[i], addr.precord->pact);
    }
}

__attribute__((constructor)) static void reg(void)
{
    initHookRegister(hook);
}
