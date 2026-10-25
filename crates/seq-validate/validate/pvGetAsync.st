/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program pvGetAsyncTest

%%#include "../testSupport.h"

#define NRUNS 100000

entry {
    seq_test_init(1);
    testDiag("start");
}

ss pvGetAsync {
    int x = 0, old_x = 0;
    assign x to "pvGetAsync1";
    int n;

    state init {
        when () {
            n = 0;
        } state get_async
    }
    state get_async {
        when (n == NRUNS) {
            testPass("pvGet completed %d times", NRUNS);
        } exit
        when () {
        } state wait_complete
    }
    state wait_complete {
        entry {
            pvGet(x,ASYNC);
        }
        when (delay(5.0)) {
            testFail("pvGet completion timeout");
        } exit
        when (pvGetComplete(x)) {
        } state get_async
        exit {
            n++;
        }
    }
}

exit {
    testDiag("exit");
    seq_test_done();
}
