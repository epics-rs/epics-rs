/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program userfuncEfTest

%%#include "../testSupport.h"

option +s;

evflag x;

%{
static void efSet_x(SS_ID ssId, EV_ID x)
{
    seq_efSet(ssId, x);
}
}%

entry {
    seq_test_init(2);
}

ss myss {
    state check_not_set {
        when (efTestAndClear(x)) {
            testFail("argh, no timeout!");
        } state set
        when (delay(0.5)) {
            testPass("cool, we came here");
        } state set
    }
    state set {
        when () {
            efSet_x(ssId, x);
        } state check_set
    }
    state check_set {
        when (efTestAndClear(x)) {
            testPass("cool, we came here");
        } exit
        when (delay(0.5)) {
            testFail("argh, timeout!");
        } exit
    }
}

exit {
    seq_test_done();
}
