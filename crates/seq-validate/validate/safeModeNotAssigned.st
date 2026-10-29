/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program safeModeNotAssignedTest

%%#include "../testSupport.h"

option +s;

int v = 0;

evflag ss2done;

#define MAX_TEST 10

entry {
    seq_test_init(MAX_TEST*2);
}

ss ss1 {
    int i = 0;
    state idle {
        when (delay(0.1)) {
            testOk(v == i, "ss1: v=%d==%d=i", v, i);
            v++;
            i++;
        } state idle
        when (v==MAX_TEST) {
        } state done
    }
    state done {
        /* C defect: a fixed delay(0.2) here ended the program while an ss2
         * scheduled that late still owed testOks; wait for its last one. */
        when (efTest(ss2done)) {
        } exit
    }
}

ss ss2 {
    state idle {
        int i;
        when (delay(0.1)) {
            if (i < MAX_TEST) {
                testOk(v == 0, "ss2: v=%d==0", v);
            }
            i++;
            if (i == MAX_TEST) {
                efSet(ss2done);
            }
        } state idle
    }
}

exit {
    seq_test_done();
}
