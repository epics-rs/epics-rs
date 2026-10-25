/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program pvGetSyncTest

%%#include "../testSupport.h"

entry {
    seq_test_init(2);
}

ss sstest {
    double x = 0;
    assign x to "pvGetSync";
    int p = 1;
    assign p to "pvGetSync.PROC";

    state stest1 {
        when () {
            int status;
            pvPut(p, ASYNC);
            testDiag("x=%f",x);
            status = pvGet(x,SYNC,0.1);
            testOk(status==pvStatTIMEOUT, "pvGet/SYNC, status=%d (%s)",
                status, status ? pvMessage(x) : "");
            testDiag("x=%f",x);
        } state stest2
    }
    state stest2 {
        when (pvPutComplete(p)) {
            int status;
            pvPut(p, ASYNC);
            testDiag("x=%f",x);
            status = pvGet(x,SYNC,10.0);
            testOk(status==pvStatOK, "pvGet/SYNC, status=%d (%s)",
                status, status ? pvMessage(x) : "");
            testDiag("x=%f",x);
        } exit
    }
}

exit {
    seq_test_done();
}
