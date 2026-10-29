/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program safeMonitorTest

%%#include "../testSupport.h"

option +s;

double cnt = 1.0;
assign cnt;
monitor cnt;
evflag ef_cnt;
sync cnt to ef_cnt;

#define MAX_TEST 10

entry {
    seq_test_init(MAX_TEST);
}

ss read {
    double on_entry;
    int n = 1;
    state react {
        when (n > MAX_TEST) {
        } exit
        when (efTestAndClear(ef_cnt)) {
            on_entry = cnt;
            testDiag("read: enter action block, cnt=%.1f", cnt);
            epicsThreadSleep(0.1);
            testOk(on_entry==cnt,
                "read: exit action block, on_entry=%.1f==%.1f=cnt", on_entry, cnt);
            n++;
        } state react
    }
}

ss write {
    state send {
        when (delay(0.04)) {
            cnt += 1.0;
            pvPut(cnt);
            testDiag("write: pvPut(cnt=%.1f)", cnt);
        } state send
    }
}

exit {
    seq_test_done();
}
