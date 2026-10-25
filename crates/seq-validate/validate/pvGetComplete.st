/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program pvGetCompleteTest

%%#include "../testSupport.h"

#define NRUNS 20000

entry {
    seq_test_init(1);
    testDiag("start");
}

ss pvGetComplete {
    int r[2];
    int w[2];
    assign r to {"pvGetComplete1","pvGetComplete2"};
    assign w to {"pvGetComplete1","pvGetComplete2"};
    int n = 0;

    state count {
        when (n < NRUNS) {
            n++;
            if (n%1000==0) {
                testDiag("passed %d runs", n);
            }
        } state test
        when () {
            testPass("%d pvGets completed in time and results agree with predictions", 2*NRUNS);
        } exit
    }
    state test {
        entry {
            int i;
            w[0] = n;
            w[1] = -n;
            for (i=0; i<2; i++) {
                r[i] = 0;
                pvPut(w[i],SYNC);
            }
            for (i=0; i<2; i++) {
                pvGet(r[i],ASYNC);
            }
        }
        when (delay(2)) {
            testFail("pvGet completion timeout");
        } state fail
        when (pvGetComplete(r[0]) && pvGetComplete(r[1])) {
            int i;
            for (i=0; i<2; i++) {
                if (r[i] != w[i]) {
                    testFail("n=%d, i=%d, w=%d, r=%d", n, i, w[i], r[i]);
                    state fail;
                }
            }
        } state count
    }
    state fail {
        when () {
        } exit
    }
}

exit {
    testDiag("exit");
    seq_test_done();
}
