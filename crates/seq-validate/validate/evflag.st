/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
/*
 * ss write: Put increasing numbers to (monitored and ef synced) PV, wait for reply,
 *  then check sent and received are equal, then sleep a bit, check again, then
 *  pvGet the anonymous PV and check again.
 * ss read: Wait for event using efTestAndClear in when clause, then send back
 *  the same value via anonymous (also monitored and ef synced) PV.
 * TODO: add a similar test with a named PV
 */
program evflagTest

%%#include "../testSupport.h"

option +s;

evflag ef_named, ef_anonymous;

double anonymous;
assign anonymous;
monitor anonymous;
sync anonymous to ef_anonymous;

entry {
    seq_test_init(44);
    efClear(ef_named);
}

ss read {
    double named;
    assign named to "evflag";
    monitor named;
    sync named to ef_named;
    state wait {
        when (efTestAndClear(ef_named)) {
            anonymous = named;
            pvPut(anonymous);
            testDiag("read: pvPut(anonymous=%.1f)", anonymous);
            epicsThreadSleep(0.1);
            testOk(named==anonymous,
                "read: named=%.1f==%.1f=anonymous", named, anonymous);
        } state wait
    }
}

ss write {
    double named = 1.0;
    assign named to "evflag";
    int n=0;
    state send {
        when (n > 10) {
        } exit
        when (delay(0.01)) {
            named = n;
            pvPut(named);
            testDiag("write: pvPut(named=%.1f)", named);
            anonymous = named;
        } state recv
    }
    state recv {
        when (efTestAndClear(ef_anonymous)) {
            testOk(named==anonymous, "write: named=%.1f==%.1f=anonymous", named, anonymous);
            epicsThreadSleep(0.1);
            testOk(named==anonymous, "write: named=%.1f==%.1f=anonymous", named, anonymous);
            pvGet(anonymous);
            testOk(named==anonymous, "write: named=%.1f==%.1f=anonymous", named, anonymous);
            n++;
        } state send
    }
}

exit {
    seq_test_done();
}
