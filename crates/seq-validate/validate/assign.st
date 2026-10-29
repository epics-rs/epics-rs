/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program assignTest

option +r;

%%#include "../testSupport.h"

int x = 0;
assign x;

#define NTESTS 3

entry {
    seq_test_init(NTESTS);
}

ss w {
    int n = 1;
    state w {
        when(n > NTESTS) {
        } exit
        when (delay(0.01)) {
            int status;
            x = !x;
            status = pvPut(x);
            testOk1(status==pvStatERROR);
            n++;
        } state w
    }
}

ss r {
    state yes {
        when (!x) {
        } state no
    }
    state no {
        when (x) {
        } state yes
    }
}

exit {
    seq_test_done();
}
