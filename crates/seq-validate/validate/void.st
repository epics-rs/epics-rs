/*************************************************************************\
Copyright (c) 2013-2015 Helmholtz-Zentrum Berlin f. Materialien
                        und Energie GmbH, Germany (HZB)
This file is distributed subject to a Software License Agreement found
in the file LICENSE that is included with this distribution.
\*************************************************************************/
program voidTest

%%#include "../testSupport.h"

int x = 42;
void *p = &x;

entry {
    seq_test_init(1);
}

ss test {
    state sizes {
        when () {
            testOk1(*(int *)p == x);
        } exit
    }
}

exit {
    seq_test_done();
}
