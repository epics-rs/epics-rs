/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program stopTest

%%#include <stdlib.h>
%%#include "../testSupport.h"

entry {
    seq_test_init(1);
}

ss main {
    state start {
        when (delay(0.5)) {
            seqStop(epicsThreadGetIdSelf());
        } state final
    }
    state final {
        when (FALSE) {
        } state final /* dummy */
    }
}

ss progress {
    state running {
        when (delay(1)) {
            testFail("still running...\n");
            seq_test_done();
            exit(0);
        } state running
    }
}

exit {
    testPass("program terminated");
    seq_test_done();
}
