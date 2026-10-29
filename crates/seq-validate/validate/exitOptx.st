/*************************************************************************\
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program exitOptxTest

%%#include "../testSupport.h"

entry {
    seq_test_init(4);
}

ss opt_x {
    int exit1cnt = 0;
    int exit2cnt = 0;
    int delayed1 = 0;
    int delayed2 = 0;
    state plus_x {
        when (delay(0.1)) {
            delayed1++;
        } state plus_x
        when (delayed1) {
        } state minus_x
        exit {
            testOk1(exit1cnt==0);
            exit1cnt++;
        }
    }
    state minus_x {
        option -x;
        when (delay(0.1)) {
            delayed2++;
        } state minus_x
        when (delayed2 > 1) {
            testOk1(exit2cnt == delayed2);
        } exit
        exit {
            exit2cnt++;
            testOk1(exit2cnt == delayed2);
        }
    }
}

exit {
    seq_test_done();
}
