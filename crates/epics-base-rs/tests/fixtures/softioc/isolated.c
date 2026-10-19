/* isolated.out is this program's run, from `cd` here, of close_links.db
 * through C iocBuildIsolated, iocRun, iocPause, iocRun and iocShutdown,
 * with init_hooks.c's probe:
 *   E=$EPICS_BASE; S=$E/modules/database/src/std/O.linux-x86_64
 *   g++ -o isolated -x c isolated.c -x c++ $S/softIoc_registerRecordDeviceDriver.cpp \
 *      -I$E/include -I$E/include/os/Linux -I$E/include/compiler/gcc \
 *      -L$E/lib/linux-x86_64 -ldbRecStd -ldbCore -lCom
 *   LD_LIBRARY_PATH=$E/lib/linux-x86_64 ./isolated $E/dbd/softIoc.dbd \
 *      2>&1 </dev/null | grep -o 'HOOK .*' */
#include "init_hooks.c"
#include "dbStaticLib.h"
#include "iocsh.h"

int softIoc_registerRecordDeviceDriver(struct dbBase *pdbbase);

int main(int argc, char **argv)
{
    if (argc != 2 || dbLoadDatabase(argv[1], NULL, NULL)
        || softIoc_registerRecordDeviceDriver(pdbbase)
        || dbLoadRecords("close_links.db", NULL))
        return 1;
    if (iocBuildIsolated() || iocRun() || iocPause() || iocRun()
        || iocShutdown())
        return 1;
    return 0;
}
