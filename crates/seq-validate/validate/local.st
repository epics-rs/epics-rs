/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
/* illustrate / test local variable declarations */
program localTest

%%#include "../testSupport.h"

/* options */
option +r;

/* variables */
int i = 1;

entry {
    seq_test_init(5);
}

/* only state-set */
ss start {
    int i = 2;
    state first {
        int i = 3;
        when (i < 5) {
            testOk(i>=3, "state local i == %d >= 3", i);
            i = i+1;
        } state last
        when () {
        } exit
    }
    state last {
        when ( delay( 0.1 ) ) {
            testOk(i==2,"state set local i==%d", i);
        } state first
    }
}

/* exit handler */
exit {
    testOk(i==1, "global i==%d", i);
    seq_test_done();
}
