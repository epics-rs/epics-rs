/*************************************************************************\
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program entryOpteTest

%%#include "../testSupport.h"

entry {
    seq_test_init(5);
}

ss opt_e {
    int entry1cnt = 0;
    int entry2cnt = 0;
    int delayed1 = 0;
    int delayed2 = 0;
    state plus_e {
        entry {
            /* only once on init */
            testOk1(entry1cnt==0);
            entry1cnt++;
        }
        when (delay(0.1)) {
            delayed1++;
        } state plus_e
        when (delayed1) {
        } state minus_e
    }
    state minus_e {
        option -e;
        entry {
            entry2cnt++;
            testOk1(entry2cnt == delayed2 + 1);
        }
        when (delay(0.1)) {
            delayed2++;
        } state minus_e
        when (delayed2 > 1) {
            testOk1(entry2cnt == delayed2 + 1);
        } exit
    }
}

exit {
    seq_test_done();
}
