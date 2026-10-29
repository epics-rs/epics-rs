/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
%%#include "../testSupport.h"

option +s;

#define NTESTS 1000

int x = 0;
assign x;
monitor x;

int i;
assign i;
monitor i;

double a[NTESTS];
assign a to {
#ifdef WITH_DB
#include "../pvSync.h"
#endif
};
monitor a;

evflag ef_x, ef_a, ef_i, ef_putx;

sync i to ef_i;

sync a to ef_a;

entry {
    seq_test_init(5*NTESTS);
    pvSync(x,ef_x);
    efSet(ef_putx);
    efClear(ef_i);
    efClear(ef_a);
}

ss pvSyncPut {
    state one {
        when (efTestAndClear(ef_putx)) {
            x++;
            testDiag("set: x=%d",x);
            pvPut(x);
        } state one
    }
}

ss pvSyncMonitor {
    state idle {
        int y = 0;
        when (efTestAndClear(ef_x)) {
            y++;
            testDiag("test: x=%d",x);
            testOk(x==y, "test: x=%d==%d=y", x, y);
            i = x-1;
            pvPut(i);
        } state idle
    }
}

ss pvSyncY {
    state pvSync {
        when (efTestAndClear(ef_i)) {
            testPass("event on i=%d", i);
            pvArraySync(a, NTESTS, ef_a);
            testDiag("after pvArraySync(a, NTESTS, ef_a)");
            a[i] = i;
            testOk1(pvPut(a[i])==pvStatOK);
            testDiag("after pvPut(a[i])");
        } state pvUnsync
    }
    state pvUnsync {
        when (efTestAndClear(ef_a) && testOk1(a[i]==i)) {
            testOk1(a[i]==i);
            if (i >= NTESTS-1) {
                state done;
            }
            pvArraySync(a, NTESTS, NOEVFLAG);
            efSet(ef_putx);
        } state pvSync
    }
    state done {
        when (delay (0.5)) {
        } exit
    }
}

exit {
    seq_test_done();
}
