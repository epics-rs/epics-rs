/*************************************************************************\
Copyright (c) 2010-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program commaOperatorTest

%%#include "../testSupport.h"

entry {
    seq_test_init(4);
}

ss myss {
    state doit {
        when () {
            int i = 0;
            /* check associativity (evaluation order) and result */
            testOk1(((i = 1), testOk1(i==1), (i = 2), testOk1(i==2),
                (i = 3)) == 3);
            testOk1(i == 3);
        } exit
    }
}

exit {
    seq_test_done();
}
