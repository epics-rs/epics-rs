/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program pvPutAsyncTest

%%#include "../testSupport.h"

int x;
assign x to "pvPutAsync1";

entry {
    seq_test_init(10);
}

ss test1 {
    state put_async {
        int i;
        entry {
            i = 0;
        }
        when (i == 5) {
        } state wait_complete
        when (delay(0.4)) {
            int status;
            x = 1;
            status = pvPut(x,ASYNC);
            /* first ok, next three failure */
            testOk((i==0)==(status==pvStatOK), "pvPut/ASYNC %d: result=%d (%s)",
                i, status, status ? pvMessage(x) : "");
            ++i;
        } state put_async
    }
    state wait_complete {
        when (delay(5.0)) {
            testFail("pvPut completion timeout");
        } state put_sync
        when (pvPutComplete(x)) {
            testPass("pvPut/ASYNC complete");
        } state put_sync
    }
    state put_sync {
        entry {
            int i = 0;
            int status;
            for (i=0; i<2; i++) {
                status = pvPut(x,SYNC);
                /* should succeed */
                testOk(status==pvStatOK, "pvPut/SYNC %d, status=%d (%s)",
                    i, status, status ? pvMessage(x) : "");
            }
            i = 2;
            status = pvPut(x,SYNC,1.0);
            /* should fail */
            testOk(status==pvStatTIMEOUT, "pvPut/SYNC %d, status=%d (%s)",
                i, status, status ? pvMessage(x) : "");

            i = 3;
            status = pvPut(x,SYNC,10.0);
            /* should succeed */
            testOk(status==pvStatOK, "pvPut/SYNC %d, status=%d (%s)",
                i, status, status ? pvMessage(x) : "");
        }
        when (delay(1)) {
        } exit
    }
}

exit {
    seq_test_done();
}
